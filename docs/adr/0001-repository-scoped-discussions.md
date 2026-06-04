# Repository-scoped discussions

Discussions are repository-scoped collaboration records with durable identity, not blobs attached to a single immutable state. This keeps Heddle's source history immutable while allowing discussions to survive captures, rebases, thread promotion, concurrent local edits, and parallel-agent coordination; state, symbol, thread, and review anchors become references on the discussion rather than the discussion's storage home.

**Status:** proposed

**Considered Options:** State-attached discussions matched the existing implementation, but made append/resolve create new states and made cross-state lookup awkward. Thread-scoped discussions fit active work units, but conversations often need to follow code after a thread lands or is reshaped.
