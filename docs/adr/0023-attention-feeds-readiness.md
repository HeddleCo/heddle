# Attention feeds readiness

Attention severity feeds `ready`, but only targeted or high-severity attention items block readiness. Resolution conflicts, visibility conflicts linked to current work or hosted sync, explicit blocker turns targeting the current thread, targeted unanswered questions, hosted rejections that affect sync or actionability, and blocked hosted-valid continuations can block; broader discussions, low-impact historical rejections, or informational items remain visible through `inbox` or diagnostics without stopping integration.

Task provenance grouping does not itself block readiness. `ready` blocks from item-level severity, target, and current-thread relevance. Grouping can summarize related attention, but it must not hide a blocker or make a whole task group block merely because one informational item is related.

The first local discussion slice should let `ready` block on blocker turns that target the current thread or actor, and on resolution conflicts that affect current work. `ready` should not block merely because a discussion is open.

A blocker turn remains readiness-impacting until a later explicit operation answers, supersedes, clears, or resolves it. Recency alone does not clear a blocker, because agents need durable coordination state rather than a best-effort notification feed.

A targeted question can stop being an attention item when a later answer cites that question, even if the discussion remains open. Clearing a question from inbox is separate from resolving the discussion lifecycle.

Context annotation conflicts should create attention, but they should block readiness only when the conflicted annotation is linked to the current thread, changed content, or an explicit policy gate. Heddle should not globally block integration because unrelated repository knowledge needs curation.

**Status:** accepted

**Considered Options:** Ignoring attention during readiness would let agents land work while unresolved coordination blockers exist. Blocking on every open discussion would make durable collaboration too noisy. Severity plus target keeps readiness honest without making discussion volume a workflow tax.
