---
status: accepted
---

# One-step capture and managed Thread checkouts

`capture` is Heddle's only public save operation for both people and agents. In
a Native Heddle Repository it advances source history directly. In Git Overlay
it records the Heddle State and writes the corresponding Git Checkpoint as one
verified operation. There is no public Heddle `commit` step, compatibility alias,
or staging/index workflow between the two.

A Capture Selection is a one-shot restriction on what the next capture reads;
it is not persistent staged state. The ordinary capture includes the whole
working tree. If the Git checkpoint cannot be written safely, preflight refuses
before the Heddle ref advances. Recovery from an interruption may complete a
missing checkpoint for the already-captured State, but must not create a second
source State.

Thread identity is independent of checkout location. Heddle derives a managed
checkout path from the Thread id when a caller does not explicitly request a
human-managed location. Agent fan-out accepts Child Thread identity and intent,
creates a managed checkout and Writer Lease for each child, and returns those
paths as results. Agents do not choose filesystem topology as part of normal
coordination.

## Consequences

- Human and agent save paths use the same `capture` verb and machine contract.
- Git Overlay capture preserves the authoritative Git tip as the checkpoint
  parent; it does not reconstruct or replace unrelated Git history.
- Help, status, schemas, recovery actions, and agent instructions do not expose
  Heddle `commit` or a staging mental model.
- Child Threads have injective Heddle-managed checkout paths, so fan-out does
  not need caller-supplied paths or a separate path-collision policy.
- Explicit checkout paths remain available on human Thread-starting surfaces
  for people who want to manage their workspace location.

## Considered options

Keeping separate `capture` and `commit` steps made Git Overlay faithfully expose
an implementation boundary, but it doubled the normal save protocol and left a
partially saved state agents had to reason about. Automatically staging through
Git's index would preserve familiar machinery while retaining invisible mutable
state. One-step capture gives both repository authorities the same durable
meaning and confines Git complexity to the Sley boundary.
