---
status: superseded
superseded-by: ADR 0046
---

# Workflow Command Vocabulary

> **Status note (2026-08, heddle#1392):** although this file is marked
> superseded, its prose still names `checkpoint` as a "public Git-facing
> milestone primitive". That is wrong: ADR 0046 (accepted) forbids exposing
> `checkpoint` and no such public verb exists in the command catalog. The
> everyday save is `capture`; `commit` is the narrow Git Overlay boundary.

This decision preferred `commit` as the everyday human save, with `capture`
as an advanced granular savepoint. Shipped CLI and ADR 0046 / ADR 0047 made
`capture` the save boundary. `commit` is the Git Overlay write to `.git`;
it is unnecessary in Native Heddle because a capture is already source
history. The text below is retained as historical decision context.

Heddle's everyday workflow vocabulary is `commit`, `ready`, `land`, `push`/`sync`, with top-level `resolve`, `continue`, and `abort` for recovery. `capture` remains a public advanced granular savepoint, and `checkpoint` remains a public Git-facing milestone primitive for agent and advanced workflows; neither is legacy. The old `ship` landing verb and bridge-oriented breadcrumbs should be retired while Heddle is alpha so the command surface reflects the intended model instead of carrying alias and compatibility complexity.

## Consequences

- `land` replaces `ship` as the long-term managed-thread landing verb.
- Human-facing guidance should prefer `commit` for everyday save work, while machine/agent guidance may use `capture` or `checkpoint` when that is the precise primitive.
- `thread refresh` and `thread resolve` should be advanced-only or absent from normal breadcrumbs in favor of top-level workflow verbs.
