# Thread work, discussions, and context

The useful agent-facing unit is a Thread with durable intent, source heads,
decisions, constraints, and evidence. A checkout is a mutable execution location
owned by one writer. Agents on the same machine can work on one Thread through
different checkouts; sharing a Thread does not mean sharing a working directory.

The new replication path implements signed, immutable source and discussion
operations, causal repair, durable acceptance, and destination-specific ongoing
sharing. Source heads converge as a set. Integration remains an explicit source
operation; metadata convergence never chooses a checkout's next HEAD. Hosted
actions still belong directly on Weft, and private checkout actions belong on
the owning Heddle device.

## What already holds up

Native collaboration has causal operations, anchored discussions, idempotent
commands, and explicit resolution conflicts. Concurrent turns are preserved;
competing resolutions are visible until a later operation resolves them.
See `crates/object-model/src/object/collaboration/{operation,materialize}.rs`.
The Thread replication ledger transports those native envelopes instead of
flattening a discussion into replaceable JSON or arrival-ordered text.

Context already distinguishes constraints, invariants, and rationale. Annotation
revision history, source anchors, visibility, supersession, and freshness make it
more useful than a transcript search result. A code binding can establish that
an annotation describes the current source; it cannot establish that its claim
is true or that a relevant test passed.
See `crates/object-model/src/object/state_context.rs` and
`crates/repo/src/repository_context.rs`.

## The context convergence gap

`union_parent_contexts` currently calls `merge_context_blobs`, which picks one
annotation by the newest current-revision timestamp, then revision ID. Two
agents can independently revise the same constraint and one complete annotation
history loses that selection. The tie-break is deterministic, but it does not
preserve concurrent knowledge. The source/discussion CRDT does not repair this.

Before enabling live context replication, represent annotation edits as causal
operations with an observed revision frontier. Preserve concurrent revisions in
a multi-value register. Explicit supersession or resolution can collapse that
frontier; wall-clock time should remain attribution metadata. Disagreement in a
constraint or invariant must remain visible to review and to an agent's work
brief. This is a proposed next contract extension, not implemented here.

## The next agent experience

Prioritize a bounded **work brief** projected from the Thread: intent, source
frontier, applicable constraints, unresolved decisions, review requirements, and
evidence with freshness and coverage. Return a pinned version and explicit
truncation/continuation. An agent should get a sufficient starting view with one
call and maintain it with one observation, rather than reconstructing it from a
timeline and many discovery calls.

Attach a durable **context-consumption receipt** to work boundaries: the brief
version, source heads, applicable annotation revisions, and evidence IDs the
agent actually received. This records provenance and supports handoff; it does
not claim the agent understood or obeyed everything. Existing local context-query
telemetry is not this receipt. Private brief contents and receipts remain under
Thread disclosure policy.

Connect resolved discussions to durable rationale, constraints, and evidence.
A decision should cite the source revision and the observation or test that
supports it. Changing relevant source can mark that evidence stale and raise a
specific unresolved requirement. This gives the next agent a precise next step
without requiring a raw transcript export.

Once these references exist, add semantic conflict previews across concurrent
source heads: competing edits, incompatible constraints, and invalidated
assumptions. A preview is evidence for an integration decision, not permission to
merge source automatically. Preserve a stable explanation and source references
so humans and agents see the same result.

## Verification and remaining integration

The current native implementation covers independent concurrent checkout
captures, durable retries, signed source/discussion admission, reordered and
interrupted ancestry, bounded peer repair, live Iroh propagation, ongoing export
opt-in, and root detachment while streaming. The owner and ordinary Biscuit
verifiers are public leaf crates in Heddle; owner-only control remains separate
from ordinary resource access.

Production CLI routing, Weft's existing account/grant enforcement, native live
Thread view projection, unknown-Thread creation, and complete source-object
transfer/install still need their adapters. The current general Thread view
materializer reads its history; large histories need indexed projections before
claiming bounded page hydration. Fixture multiplexing demonstrates transport
stream capacity only, not the throughput of these durable operations.
