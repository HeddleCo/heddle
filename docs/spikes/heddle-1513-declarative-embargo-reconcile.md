# heddle#1513 — Declarative embargo publication

**Status:** spike recommendation for #319; no implementation in this PR.
**Inputs:** [#319](https://github.com/HeddleCo/heddle/issues/319), especially the
2026-06-05 architect review; [#516](heddle-516-embargo-frontier.md) for frontier
and promotion semantics; [#266](heddle-266-commit-visibility-tiers.md) §5 for
the existing downward-closed visibility rule.

## Decision

Replace the proposed `resolve_frontier` publish funnel with one complete desired
publication, then reconcile each destination against its actual refs:

```text
snapshot = confirmed native roots + visibility facts + mappings + placement facts
servedFrontier = resolve_frontier(snapshot, audience)
desired_refs = project(servedFrontier, audience)
plan = reconcile(desired_refs, actual_refs, previously_exported_refs)
materialize served objects; apply plan with expected-old refs; record confirmed ownership
```

`project` is pure over a complete, versioned snapshot. It returns **all** managed
branch, exact-tag, synthetic-frontier, and notes intentions, plus a `HEAD`
disposition and the served object roots. It performs no ref writes and never
reads a mutable Git mirror to decide what should be public. Notes initially
appear as canonical entries in that value; deterministic, parentless encoding
turns them into the `refs/notes/heddle` OID before reconciliation. Every outward
destination, including the internal mirror, local bare export, network push, and
post-import publish, consumes this same value. A missing surface cannot be
silently skipped by a later enforcement pass because it has no independent ref
constructor.

The **visibility rule stays as designed**: `served(v, A) = visible(v, A) &&
all(parents(v) are served)`. A directly Public tip above a Private ancestor is
`REDACT` for the public audience: neither that tip, its state note, nor its Git
object closure is a publication root. The branch remains at its last served
frontier, or is absent if none exists. There is no stub commit, reparenting, or
rewrite of an already published commit. A missing state, mapping, or confirmed
visibility generation blocks a new projection. An absent per-state sidecar means
initially Public only when that confirmed generation proves the fact set is
complete. This note changes publication construction, not audience or
authorization policy.

## Why this is needed on `origin/main`

The issue's `crates/cli/src/bridge/*` paths moved to
`crates/git-projection/src/*`. The intended downstream model already exists in
`git_core.rs:2794` as `plan_destination_reconcile`: it diffs the whole desired
ref set against destination tips and Heddle's per-destination exported-ref
record, deriving writes, force, deletes, and ownership. It protects foreign refs
by checking the recorded name and last published OID. Both local-path export
(`git_core.rs:914-973`) and network push use it.

Upstream `export_scoped` (`git_export.rs:422`) still mutates the mapping,
appends and removes notes, projects only heads/tags/stored synthetic refs
(`project_desired_refs`, `git_export.rs:1371`), and then runs separate head, tag,
and synthetic reconciliation. The notes ref is collected afterward from the
mirror, and `HEAD` is managed elsewhere. In `git_notes.rs`, `write_note` and
`remove_notes` create notes commits descending from the old notes head, leaving
previously written embargoed note blobs reachable through history. The current
destination planner treats notes as branch-like fast-forward history, which
cannot safely apply a parentless notes rebuild. `collect_managed_ref_updates`
also treats every `refs/notes/*` ref as managed without a recorded target.
These are the remaining seams the full desired publication must close.

The existing `project_desired_refs` and `plan_destination_reconcile` are starting
points, not a second projection to retain alongside a new one. Move ownership
of **which** refs exist into the complete projection, then reuse or factor the
destination planner's single desired/actual decision for the mirror too. Keep
destination-specific actual tips and owned OIDs in reconciliation. The mirror is
one destination, never the source of truth for later destinations.

## Reconciliation contract

The desired value carries full ref names and exact target identities, canonical
notes entries, the chosen symbolic `HEAD` target (or unborn disposition), and
the complete served object roots. A single materialization step writes only
objects reachable from those roots, constructs the notes OID from an empty tree,
and hands a concrete desired map to the existing planner. It must validate that
the source generation still matches before any ref update. Neither a cached
mapping nor an imported Git OID proves servedness on its own.

For each destination, the planner examines `desired ∪ previously_exported`, not
all `refs/heads|tags|notes/*` at that destination. An absent desired owned ref
is a delete; a different desired target is a create, fast-forward, or guarded
force. Force and delete require `actual == last_published` or the user's explicit
force authority. A foreign ref, including one at a Heddle commit OID, remains
untouched. Apply with expected-old CAS; advance the destination's ownership
record only after confirmed writes/deletes. A partial remote update is retried
by re-reading actual refs and recomputing the same complete plan.

The current `creatable_names` gate makes a scoped push avoid creating new
out-of-scope siblings. #319 asks for scoped/full publication equality. The
target contract makes scope an optimization of object work only: the same
audience and native snapshot produce the same desired managed refs and final
published set in both modes. Removing that first-create distinction is an
intentional behavior change for the implementation owner to confirm before the
relevant slice. Until then, a test must at least compare the complete desired
projection and verify that every previously published out-of-scope ref is
reconciled; scope must never weaken servedness.

## Surface map and conformance

Each row is an assertion against the **published destination**, not just the
internal mirror. Tests use a history `A(Public) -> B(Private) -> C(Public)` for
public-audience `REDACT`, plus public sibling and foreign-ref fixtures where
applicable.

| #319 surface | Desired publication and reconcile rule | Conformance assertion |
|---|---|---|
| Branch ref-sync | Project `refs/heads/<thread>` at the stable served primary frontier; omit when no ancestor serves. Reconcile with CAS; ordinary advances are FF-only. A `Set` that narrows an already served tip needs #516's owner-policy decision before claiming a forward-only contract. | With B Private from inception, the destination head stays at A and cannot reach B or C. A foreign divergent branch survives. Promotion advances to C without rewriting A. |
| Marker → tag | Project an exact marker target only if that exact state serves; never lag the tag to A. Reconcile deleted/retargeted owned tags by name. | A marker at C yields no public tag while B is Private; an existing owned tag is deleted, a foreign tag survives, and promotion creates the exact C target. |
| State notes | Project notes only for served mapped states reachable from desired commit roots. A note cannot make an otherwise withheld state visible. | The destination notes tree has no B/C entry, including for an orphaned or out-of-thread mapping; after promotion it has the eligible entries. |
| History-bearing notes rebuild | Encode the complete desired notes set in a deterministic parentless notes commit, or omit the ref when empty. Reconcile the owned notes ref as an expected-old, non-FF replacement; never copy the old notes commit as a parent. | Start with a previously published B note, withhold B, export again, and walk **every object reachable from the destination notes ref**: no B note blob or old notes ancestor is reachable. A foreign/diverged notes tip is not overwritten. |
| `HEAD` symref and bulk export/push | Project `HEAD` to a branch present in the same desired map, or an unborn state. Bulk object copying and ref enumeration consume only the desired roots/plan. Local bare export writes HEAD after its target is confirmed; a network host must validate/update its default symref through host control, since Git push does not set remote HEAD. | Both local bare export and network push contain no raw B/C ref or object root; clone `HEAD` resolves to a served branch, or stays unborn. A failed ref CAS cannot leave HEAD pointing at a missing branch. |
| Multi-root merge frontier | Project one stable primary branch plus every otherwise unreachable served sibling under `refs/heddle/frontier/<thread>/<full-state-id>`; delete redundant owned siblings after a served merge. Heddle-aware fetch includes this reserved namespace. | For an embargoed two-parent merge, both public parents are fetchable and no hidden state is reachable; after promotion the merge becomes primary and stale synthetic refs disappear at destination. A raw Git write to `refs/heddle/*` is rejected by the host receive gate. |
| Scheduled promotion | The authority commits a superseding Public `StateVisibility` and `StateVisibilityPromote` before computing a broader desired set; serve/export hosts use only the acknowledged persisted generation. | Before due materialization, refs/notes stay withheld even after wall-clock passes. After record propagation/ack, each destination converges; replay adds no second promotion, and a lagging host keeps its last confirmed plan or blocks. |

The reserved-ref name above follows #516's collision correction: a `ChangeId`
can survive a rewrite and does not uniquely name a `State`. #319 requests a
`<full-changeid>` suffix; the implementation owner must ratify the full
`StateId` spelling (or another injective spelling) before landing the
multi-root slice. The reserved namespace remains `refs/heddle/frontier/*`, never
`refs/heads/*`; a Heddle-aware clone fetches it with an explicit refspec.

The raw Git receive gate belongs to the hosting boundary: the CLI cannot guard
an independent Git server's `receive-pack`. That host rejects raw writes to
`refs/heddle/*`, while bridge publication uses its authorized path. The fixture
must exercise a real raw `git push` denial, not only local name validation.

For all rows, compare scoped and full projections from the same generation and
audience. Also assert that a ref retracted after an earlier export is deleted
from the **destination**, while an unrelated foreign ref remains. A structural
test should prohibit outward writers from taking raw thread/marker/mapping OIDs
or enumerating all mirror refs as their publication source. The raw-OID test in
#319 remains useful, but the stronger boundary is a single constructor for the
complete desired publication.

## First one-PR implementation slice

**History-clean notes as one desired ref.** Limit this slice to the existing
linear public projection. Change
`crates/git-projection/src/git_export.rs`, `git_notes.rs`, and `git_core.rs`,
with `export_rebuilds_notes_without_hidden_history` in the existing
`git_export.rs` test module. Extend the existing desired projection
with canonical notes entries and the resulting `refs/notes/heddle` target;
replace the export-time `write_note`/`remove_notes` sequence with one rebuild
from served state identities. Route the mirror and destination notes update
through the same owned, expected-old reconciliation contract as the other
desired refs. Do not add the multi-root resolver, scheduler, or a new audience
policy in this PR.

The **failing-first conformance test** publishes a B note, then changes B to
Private while C stays directly Public, exports again to an already-exported
bare destination, and traverses its `refs/notes/heddle` commit/tree/blob
closure. Before the fix it finds B's old note through the previous notes commit
even though the tip tree has been filtered. After the fix, no B/C note object
is reachable; an unrelated foreign ref survives; a repeated export is
idempotent. The test also verifies the new
notes ref is parentless and that an out-of-band notes tip is preserved/reported
rather than force-clobbered. Run the affected crate's targeted `cargo nextest`
test, build, and clippy, plus the CLI regression job if this slice changes CLI
behavior. Capture the red and green outputs in that PR.

Later owner-confirmed slices: complete branch/tag/synthetic projection and
scoped/full equality; stable multi-root cursor, namespace/fetch and receive
gate; HEAD and every bulk/import/push entry point; scheduled-promotion authority
and propagation. #319 remains open until every row and destination passes its
conformance assertion.

## Existing policy questions carried forward

#516 §10 records unresolved post-serve narrowing, hosted promotion authority,
and durable publication cursor ownership. Current `visibility set` can narrow
an already served state, although #266 says serving is forward-only; ref
retraction cannot recall bytes already fetched. The reconciler must handle
today's narrowing conservatively and cannot claim the irreversible disclosure
problem solved. This spike does not choose a new visibility rule or silently
invent a multi-host cursor.
