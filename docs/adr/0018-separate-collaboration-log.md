# Separate collaboration log

Heddle stores collaboration operations in a separate repository collaboration log, linked to source states or oplog entries where relevant. The existing oplog remains focused on undo/redo and source, worktree, and repository mutations; collaboration operations have different sync, query, retention, conflict, and attention semantics.

**Status:** accepted

**Considered Options:** Reusing the existing oplog would reduce storage surfaces, but it would couple discussion turns and context edits to source-history undo behavior. A separate collaboration log can index by discussion, anchor, thread, actor, attention target, and causal parents while still referencing source history when needed.
