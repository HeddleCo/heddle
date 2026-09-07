---
status: accepted
---

# First-class conflicted States

A source conflict is durable Heddle data, not punctuation embedded in source
bytes. A conflicted State is an ordinary immutable source-history State with a
non-empty Conflict Set attachment. It may be captured, synced, inspected,
extended, and used as a merge parent. It may not become ready, land, or project
as an ordinary Git commit while source conflicts remain unresolved.

Each conflict has a stable episode identity and immutable versions. A version
contains its anchor, base, status, and a deduplicated set of attributed Conflict
Candidates. Candidates identify content, source State, source Thread, producer,
and their relationship to prior candidates. They have no durable `ours` or
`theirs` direction; those are presentation labels relative to a viewer.

The checkout materializes conflict-free merge results plus a marker-free Working
Candidate for each unresolved region. Editing and capturing that file adds an
attributed candidate; it does not silently resolve the conflict. Resolution is
an explicit attributed successor operation that cites the exact Conflict Version
it considered and either selects a candidate or records a synthesized one. If
the cited version is stale, resolution fails closed and creates attention.

## Consequences

- Conflict Sets and candidates are content-addressed objects transferred through
  the same State attachment graph as other Heddle metadata.
- `status`, `diff`, `show`, `resolve`, review, and Tapestry render the same
  underlying conflict object with progressive disclosure.
- Multiple agents may build on a conflicted State or add candidates without
  sharing a writable checkout.
- Resolution records preserve the resolver, method, evidence, and exact input
  version. Concurrent incompatible resolutions remain explicit conflicts.
- Git conflict markers are an import/export compatibility adapter only. They are
  never canonical Heddle source and are not the default checkout representation.

## Considered options

Persisting a marker-filled tree plus a sidecar list keeps the merge engine close
to Git, but makes invalid program text canonical, assigns unstable directional
meaning, and forces agents to parse presentation bytes. A mutable conflict row
would simplify updates but lose content-addressed history and make stale
resolution races invisible. Immutable versioned candidate sets preserve both
ordinary editing and forensic provenance.
