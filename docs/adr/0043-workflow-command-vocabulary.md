---
status: superseded
superseded-by: ADR 0052
---

# Workflow Command Vocabulary

This decision preferred `commit` as the everyday human save, with `capture`
as an advanced granular savepoint. ADR 0052 replaced that split with one-step
capture for both repository authorities. The text below is retained as
historical decision context.

Heddle's everyday workflow vocabulary is `commit`, `ready`, `land`, `push`/`sync`, with top-level `resolve`, `continue`, and `abort` for recovery. `capture` remains a public advanced granular savepoint, and `checkpoint` remains a public Git-facing milestone primitive for agent and advanced workflows; neither is legacy. The old `ship` landing verb and bridge-oriented breadcrumbs should be retired while Heddle is alpha so the command surface reflects the intended model instead of carrying alias and compatibility complexity.

## Consequences

- `land` replaces `ship` as the long-term managed-thread landing verb.
- Human-facing guidance should prefer `commit` for everyday save work, while machine/agent guidance may use `capture` or `checkpoint` when that is the precise primitive.
- `thread refresh` and `thread resolve` should be advanced-only or absent from normal breadcrumbs in favor of top-level workflow verbs.
