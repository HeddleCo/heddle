# Collaboration compensation, not source undo

`heddle undo` does not erase collaboration operations by default. Collaboration records use compensating operations such as reopen, supersede, visibility change, or later correction turns; source undo can create attention when it invalidates a discussion resolution, but it does not delete the discussion history.

Migration into the repository collaboration log follows the same rule. Migration can support dry-run and can leave legacy objects untouched, but once repository collaboration operations are written, reversal is a compensating collaboration operation or local imported-history cleanup before sync, not source-history undo.

**Status:** accepted

**Considered Options:** Letting source undo reverse collaboration changes would make some mistakes easy to recover from, but it would weaken append-only discussion auditability and couple two different histories. Explicit compensating operations preserve provenance while still allowing the current collaboration view to change.
