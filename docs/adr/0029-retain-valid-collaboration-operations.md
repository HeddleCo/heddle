# Retain valid collaboration operations

Locally valid collaboration operations are retained in v1; Heddle does not garbage-collect durable discussion, context, or attention history by default. Operations that Weft rejects under hosted policy remain in the local collaboration log and are marked through sync metadata rather than deleted or rewritten. Cleanup can remove incomplete temporary artifacts and rebuild disposable indexes or materialized views, but retention policy for valid collaboration history is deferred.

**Status:** accepted

**Considered Options:** Pruning old collaboration operations would limit repository growth, but it would weaken auditability, append-only turns, compensating operations, capability history, and agent coordination provenance. Retention should be introduced later as explicit policy, likely with Weft namespace support.
