# Repository collaboration log

Heddle collaboration data is organized as a repository collaboration log made of independently addressable records, with discussions as the first record type. This avoids making the whole discussion one coarse CRDT object or making each turn its own top-level CRDT; turns, resolutions, visibility, and anchors can merge under the durable identity of one discussion while future collaboration records share the same repository-level sync shape.

**Status:** accepted

**Considered Options:** Whole-discussion CRDTs were simpler but too coarse for concurrent append, resolve, and anchor changes. Per-turn CRDTs were precise for message ordering but too narrow for discussion metadata and future collaboration records.
