# Context annotations as collaboration records

Context annotations are repository collaboration records, while source states preserve context snapshots for provenance and replay. Editing or superseding durable knowledge should not advance source history; immutable states still record which context view was known when the state was created.

V1 context annotations should use operation-based create and supersede semantics rather than editable document CRDT text. Concurrent incompatible annotation updates should materialize as explicit ambiguity or conflict instead of silently merging prose into canonical guidance.

Context extraction from discussions can ship after the first local discussion slice. Decision turns do not automatically become context annotations; extraction remains an explicit workflow that creates or updates context annotation records.

When context extraction creates or updates a context annotation, the annotation operation should cite the source discussion operations it was extracted from. Those references preserve provenance without making the original discussion turns themselves canonical context.

**Status:** proposed

**Considered Options:** Keeping context annotations only on source states matched the current implementation, but it makes durable knowledge edits behave like source-history mutations. Moving live context to the collaboration log keeps annotations mergeable and queryable while preserving historical context views on states.
