# Heddle-native collaboration store

The local collaboration log uses Heddle-native content-addressed storage, append/index files, and rebuildable materialized views rather than SQLite or another embedded database as the source of truth. Collaboration indexes can optimize lookup by discussion, anchor, thread, actor, attention target, and causal parents, but operation objects remain the durable record. `fsck` verifies operation integrity and references; `doctor` can diagnose and rebuild disposable indexes and views.

`fsck` recomputes every `CollabOpId` from canonical operation bytes as the primary durable-object integrity check. It also verifies shard path placement, supported schema versions, causal references, and whether indexes and views can be rebuilt.

`doctor` may automatically rebuild disposable collaboration indexes and materialized views. It does not repair durable operation bytes by guesswork. If operation bytes are invalid or missing, `doctor` reports or quarantines the problem and points to restore or explicit import rather than synthesizing history.

**Status:** accepted

**Considered Options:** SQLite would make querying and indexing straightforward, but it would introduce a second local source-of-truth model for core OSS behavior. Heddle-native storage keeps collaboration aligned with content addressing, fsck/doctor repair, and the longer-term direction of owning core dependencies.
