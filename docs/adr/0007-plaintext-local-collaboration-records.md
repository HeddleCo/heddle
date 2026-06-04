# Plaintext local collaboration records

Heddle stores local collaboration records in plaintext for v1 and applies capability-aware filtering in the CLI, while Weft enforces hosted access on sync and API boundaries. This keeps local-first search, semantic anchoring, CRDT reconciliation, and agent workflows tractable; restricted visibility does not claim protection from someone with direct filesystem access.

Redaction suppresses content from normal materialized views and sync behavior; it is not the same as physical purging of durable local bytes. Physical purge or encrypted-at-rest storage should be a separate explicit retention/security workflow rather than implied by collaboration visibility or redaction labels.

Creating a redaction is policy-sensitive. Local Heddle should require an active capability that permits redaction for the target record, and hosted sync should let Weft accept or reject the redaction under hosted policy. Redaction commands should require exact operation targets and a reason.

Default human and JSON output for redacted content should show a redaction marker, target operation ID, visible reason or reason code, and actor or policy metadata allowed by the active capability. It should not include suppressed content unless the caller uses an explicit privileged diagnostic or forensic mode.

**Status:** proposed

**Considered Options:** Encrypting restricted collaboration records locally would provide a stronger security story, but it would require key distribution and merge/search semantics that are not necessary for the first world-class OSS CLI. If local unreadability becomes a requirement, it should be introduced as an explicit encrypted storage mode rather than implied by visibility labels.
