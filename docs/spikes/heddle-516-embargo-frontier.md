# heddle#516 — Embargo frontier computation and scheduled promotion

**Status:** design decision for #319 implementation. **Scope:** public/audience-
scoped ref publication, multi-root frontier computation, history-clean state-note
publication, and scheduled `embargo_until` promotion. This note does not change
production code and does not design the separate object-transfer protocol work in
#318.

---

## 1. Verdict

Implement one native DAG resolver and one sealed publication-plan adapter:

1. `repo::resolve_frontier` computes direct visibility, downward-closed
   servedness, the maximal **served** antichain, and the minimal **unserved** cut
   for all logical roots in one snapshot.
2. `git_projection::resolve_publication` is the only constructor of a sealed
   `ResolvedPublication`. It projects the native result into heads, exact tags,
   synthetic frontier refs, a freshly rooted notes ref, `HEAD`, and a managed
   destination delete-set.
3. Every export, push, and post-import sync path passes that plan to ref writers.
   No writer accepts raw repository refs, raw mapped OIDs, or an independently
   computed visibility boolean.
4. `refs/notes/heddle` is a deterministic, parentless full rebuild. Incremental
   append/removal is not a safe publication algorithm because old notes remain
   reachable through the notes commit's parents.
5. A schedule is an advisory trigger. A designated authority materializes a
   signed superseding `Public` record and `StateVisibilityPromote` operation under
   the repository write lock before a broader publication. Replicas never compare
   `embargo_until` with their own clocks.

Two phrases from the issue need correction in implementation:

- The useful publication frontier is the antichain of **maximal served states**.
  “Maximal embargoed states” normally selects hidden tips and is not safe to
  publish. This note also computes the **minimal unserved cut**, which is the
  embargo boundary useful for diagnostics and schedule work.
- A full `ChangeId` is not a collision-proof state name. `ChangeId` is a
  rewrite-stable 16-byte identity (`crates/object-model/src/object/hash.rs:97-129`),
  and rebase deliberately copies it to a newly minted `State`
  (`crates/cli/src/cli/commands/rebase/rebase_ops.rs:480-488,641-647`). Synthetic
  frontier refs therefore use the full `StateId`, the content identity defined at
  `crates/object-model/src/object/hash.rs:161-195`.

## 2. Ground truth in this checkout

### 2.1 Visibility is already a signed per-state supersede chain

`StateVisibility` already contains exactly the substrate this design consumes:
`state`, `tier`, advisory `embargo_until`, `declarer`, `declared_at`, `signature`,
and `supersedes` (`crates/object-model/src/object/state_visibility.rs:26-54`). The
per-state sidecar is the versioned `StateVisibilityBlob { format_version,
records }`, one rmp-serde file per state
(`crates/object-model/src/object/state_visibility.rs:124-142`). Its effective
record follows the supersede graph; a concurrent fork resolves to the
lexicographically greatest content hash
(`crates/object-model/src/object/state_visibility.rs:159-205`). Tier ordering is
defined by `VisibilityTier` and its restrictiveness rank
(`crates/object-model/src/object/visibility_tier.rs:16-39,56-92`). Audience
matching already lives in the repository visibility module
(`crates/repo/src/visibility.rs:31-48,148-184`). The frontier resolver calls that
logic; it does not re-encode tier policy.

A `State` has a vector of parent `StateId`s
(`crates/object-model/src/object/state_core.rs:216-230`), including ordinary
multi-parent merges (`crates/object-model/src/object/state_core.rs:333-368`). No
merge-base calculation belongs in the frontier algorithm.

The current combined mutation primitive serializes a visibility write and its
oplog append under the repository write lock. `Set` always commits, while
`Promote` requires a strictly less restrictive tier
(`crates/repo/src/repository_state_visibility.rs:313-390`). The proposed
scheduler must extend that primitive rather than assemble a sidecar write and an
oplog append at its call site.

### 2.2 Three substrate contradictions must not be papered over

**Public promotions are currently erased.** The sidecar writer removes the whole
per-state sidecar when the effective head is `Public`
(`crates/repo/src/repository_state_visibility.rs:233-250`). That preserves the
local “absence means initially public” optimization, but it destroys the signed
superseding fact that a replica must receive. Scheduled promotion requires this
rule to change: an initial state with no declarations may remain
public-by-absence, but a `Public` record that supersedes a non-public record MUST
remain in the `StateVisibilityBlob` and replicate. The existing wire accept/load
surface transports persisted records and never applies a clock
(`crates/repo/src/repository_state_visibility.rs:464-523,542-562`); it cannot
propagate a record that the writer deleted.

**The CLI cannot create a schedule today.** `visibility set` and `visibility
promote` expose no `--until` argument
(`crates/cli-args/src/cli/cli_args/commands_visibility.rs:82-107`), and both
commands construct records with `embargo_until: None`
(`crates/cli/src/cli/commands/visibility.rs:59-91,106-142`). This note defines
schedule execution, but a schedule-creation surface remains follow-up work.

**Visibility narrowing is possible today.** Only `Promote` applies the monotonic
rank check; `Set` may install a more restrictive head
(`crates/repo/src/repository_state_visibility.rs:349-385`). Therefore #319 cannot
assume “served once means served forever” without a new durable served ledger and
a policy change. The publication plan specified here safely retracts managed refs
when current facts narrow visibility, while acknowledging that already delivered
objects cannot be recalled. The owner-policy question is recorded in §10.

## 3. Terms and invariants

For audience `A`, let `direct_visible_A(v)` be the result of the existing tier and
audience policy for state `v`. An advisory timestamp does not participate in this
predicate. A due schedule changes the predicate only after §8 has persisted its
superseding record.

For a complete ancestor DAG:

```text
served_A(v) = direct_visible_A(v)
              AND for every parent p of v: served_A(p)
```

The empty parent conjunction is true. The served set is downward-closed: if a
state is served, every transitive parent is served.

Across the union ancestry of all roots:

- `F*`, the **batch publication frontier**, is every served state with no served
  child in the union. It is the maximal antichain of the batch's downward-closed
  served set and is the minimal root set needed to make all served objects
  reachable.
- `C*`, the **batch hidden cut**, is every unserved state with no unserved parent
  in the union.

For each logical root `r_i`, the same definitions are restricted to that root's
ancestor closure:

- `F_i`, the **publication frontier**, is every served ancestor of `r_i` with no
  served child in `r_i`'s ancestor closure. It is the maximal antichain of the
  downward-closed served set.
- `C_i`, the **hidden cut**, is every unserved ancestor of `r_i` with no unserved
  parent in `r_i`'s ancestor closure. It is the minimal antichain at which hidden
  paths enter the embargoed region.

The invariants are:

1. Every object reachable from a published commit ref is served for the plan's
   audience.
2. Every maximal served state for a thread remains reachable through either the
   thread's primary branch or a synthetic frontier ref.
3. An exact marker/tag is never retargeted to an ancestor. It is present only when
   its exact target is served.
4. A state note exists in the published notes tree only for a served mapped state
   reachable from the plan's desired commit refs.
5. `HEAD` either resolves to a branch in the same plan or is unborn because the
   plan contains no served branch.
6. A missing state, parent, visibility generation, mapping, or publication cursor
   fails closed. It is never interpreted as public.
7. Scoped and full publication use the same complete mapped-root set and the same
   servedness result. Scope changes only which user-facing roots are selected, not
   the downward-closure calculation.

The current exporter violates invariant 6: its servedness walk treats a missing
state as if its parents were served
(`crates/git-projection/src/git_export.rs:1194-1227`). The implementation must
distinguish an explicitly trusted shallow boundary from corruption or an
incomplete replica.

## 4. Multi-root frontier algorithm

### 4.1 Inputs and outputs

Inputs are resolved under one repository read snapshot:

- `roots[0..k)`: logical root key plus exact `StateId`. Roots include every native
  thread selected for publication and every mapped state needed by exact tags and
  notes. Duplicate `StateId`s are allowed; logical root keys remain distinct.
- `audience`: the existing `AudienceTier`/scope used by direct visibility.
- `shallow_boundary`: an explicit set of parent `StateId`s whose ancestry is
  intentionally absent and already trusted for this audience. Ordinary missing
  objects are errors.
- `visibility_generation`: a confirmed token `(epoch, manifest_hash)` from the
  authority. `manifest_hash` is a domain-separated hash of the StateId-sorted
  entries `(StateId, sorted content hashes of every record in that
  StateVisibilityBlob)` for every persisted sidecar. Sorting record hashes makes
  the token independent of cross-host arrival order; `epoch` orders additions and
  deletions that might otherwise recreate a prior manifest. A single-host
  repository computes the manifest under its repo lock. A hosted replica may use
  it only after acknowledging the authority's exact epoch and manifest. This token
  does not exist in the current tree and is part of the
  propagate-before-publish follow-up, not a value a caller may invent.

Outputs are:

- the union DAG and deterministic parents-before-children order;
- `direct_visible[v]` and `served[v]`;
- batch `frontier = F*` and `hidden_cut = C*`, plus per-root
  `frontier[i] = F_i` and `hidden_cut[i] = C_i`;
- the union of served states and a generation digest covering roots, graph,
  the confirmed visibility generation and visited effective heads, audience,
  mapping generation, and shallow-boundary identity.

The resolver does not choose Git ref names or write objects.

### 4.2 Pseudocode

`StateId` raw bytes are used only for deterministic queue and output ordering.
They do not decide graph reachability or an existing branch's placement.

```text
resolve_frontier(roots, audience, shallow_boundary):
    # Phase 1: load the union ancestry graph exactly once.
    V = empty map StateId -> State
    E = empty parent->child adjacency
    pending = min_queue(StateId raw bytes)
    pending.add(each roots[i].state)

    while pending not empty:
        v = pending.pop_min()
        if v in V: continue
        state = load_state(v)
        if state is missing:
            error IncompleteFrontierInput(v)
        V[v] = state
        for p in state.parents:
            E.add(p -> v)
            if p not in shallow_boundary:
                pending.add(p)

    # Explicit shallow parents act as already-served boundary vertices. They
    # have no invented metadata or parent edges.
    add each referenced shallow_boundary vertex to V as SHALLOW_SENTINEL

    topo = kahn_parents_before_children(V, E,
                                        ready_tiebreak=StateId raw bytes)
    if topo does not contain all V: error CorruptStateCycle

    # Phase 2: record which logical roots contain each vertex in their ancestry.
    reach[v] = zero bitset of k bits for every v
    for i in 0..k: reach[roots[i].state].set(i)
    for child in reverse(topo):
        for parent in parents(child):
            reach[parent] |= reach[child]

    # Phase 3: least fixed point of visible AND all parents served.
    for v in topo:
        if v is SHALLOW_SENTINEL:
            direct_visible[v] = true
            served[v] = true
        else:
            direct_visible[v] = effective_visibility(v, audience)
            served[v] = direct_visible[v]
                        AND all(served[p] for p in parents(v))

    # Phase 4: extract both antichains with bitset operations. Iterating
    # individual root bits happens only for actual output members.
    batch_frontier = empty list
    batch_hidden_cut = empty list
    frontier = [empty list; k]
    hidden_cut = [empty list; k]
    for v in topo:
        served_child_roots = zero bitset of k bits
        for child in children(v):
            if served[child]: served_child_roots |= reach[child]
        if served[v] AND no child of v is served:
            batch_frontier.push(v)
        frontier_roots = reach[v] AND NOT served_child_roots if served[v]
                         else zero bitset
        for i in set_bits(frontier_roots):
                frontier[i].push(v)

        unserved_parent_roots = zero bitset of k bits
        for parent in parents(v):
            if NOT served[parent]: unserved_parent_roots |= reach[parent]
        if NOT served[v] AND no parent of v is unserved:
            batch_hidden_cut.push(v)
        cut_roots = reach[v] AND NOT unserved_parent_roots if NOT served[v]
                    else zero bitset
        for i in set_bits(cut_roots):
                hidden_cut[i].push(v)

    sort batch_frontier, batch_hidden_cut, and every per-root list by StateId bytes
    return graph, topo, served,
           batch_frontier, batch_hidden_cut, frontier, hidden_cut,
           generation_digest
```

Shallow sentinels are proof-carrying inputs, not a general missing-parent
fallback. They are omitted from returned frontiers and output object sets. If a
root itself is missing, or an absent parent was not named in the trusted shallow
boundary, resolution fails.

### 4.3 Correctness and merge behavior

The topological pass is the least fixed point because every parent is decided
before its child. If a hidden state has a directly visible descendant, the
descendant remains unserved through the parent conjunction. Consequently a served
descendant implies a served path back through every parent, so testing immediate
children is sufficient to find maximal served vertices.

Multiple roots share one union walk but retain membership through `reach`. A merge
may belong to several root closures; it is evaluated once and may contribute to
several results. An embargoed two-parent merge leaves the maximal served state on
each visible parent line in `F_i`. Octopus merges merely produce a larger
antichain. Criss-cross histories need no merge-base selection. When the merge and
all of its ancestors later become served, the merge dominates those parents and
the antichain collapses to the merge.

### 4.4 Complexity

Let `V` and `E` be the state and parent-edge counts in the union ancestry, `k` the
logical-root count, `w` the machine word size, and `L` the total number of
visibility records decoded while resolving effective heads.

- Time: `O((V + E) * ceil(k / w) + L + output)` with dense root-membership
  bitsets. Loading, topological servedness, and a single-root request are linear.
- Memory: `O(V * ceil(k / w) + V + E)` plus decoded records and output.

For very sparse root membership the implementation may replace dense bitsets with
sorted small sets/Roaring bitmaps, but that is a representation optimization; it
must preserve the same StateId-sorted output. The resolver must not run one full
ancestry walk per root.

## 5. Placement of the `resolve_frontier` chokepoint

### 5.1 Two layers, one decision

The native resolver belongs in `crates/repo`, for example
`visibility_frontier.rs`, because Git and #318's wire planner must share the exact
servedness definition. Its API returns native identities and proof metadata only.

The sole Git publication constructor belongs in `crates/git-projection`, for
example `publication.rs`:

```text
resolve_publication(repo_snapshot,
                    audience,
                    selected_logical_refs,
                    previous_publication)
    -> ResolvedPublication
```

`ResolvedPublication` has private fields and contains:

- the native `FrontierResolution` and generation digest;
- desired managed branch refs and exact marker/tag refs;
- desired synthetic frontier refs;
- the deterministic rebuilt `refs/notes/heddle` OID or deletion;
- desired `HEAD` disposition;
- every served `StateId` and mapped Git OID permitted as an emission root;
- `managed_now`, and for each destination
  `delete = managed_previously_exported - managed_now`.

Low-level mirror, filesystem-export, and network-push writers accept only
`&ResolvedPublication`. Raw ingest writers accept a different `IngressRefs` type
that cannot be converted into a publication plan. The resolver reads under a
consistent repository snapshot. Immediately before ref publication, the writer
compares the plan generation with current raw roots, visibility heads, mappings,
audience, and note format. A mismatch discards the whole plan and resolves again.

This is a structural gate, not a convention. The existing exported-ref manifest
already records per-destination Heddle ownership
(`crates/git-projection/src/git_core.rs:3289-3330`); the plan uses that record for
deletions and never diffs the entire destination namespace. Foreign destination
refs are never in `managed_previously_exported` and therefore survive.

### 5.2 Actual call sites that must route through the plan

| Surface | Current call site / behavior | Required route |
|---|---|---|
| CLI export | `cmd_git_export` reaches the export path at `crates/cli/src/cli/commands/git_projection_io.rs:344-400`. | Resolve one plan, materialize its objects/notes, then reconcile only its desired refs and managed delete-set. |
| CLI import | `cmd_git_import` calls the importer at `crates/cli/src/cli/commands/git_projection_io.rs:448-498`; `import_git_repository` imports refs and notes at `crates/git-projection/src/git_ingest.rs:20-52,149-195`. | Treat imported refs as ingress only. After native import, resolve one plan before anything becomes a public/managed mirror ref. Direct imported note writes must not become the published notes ref. |
| CLI sync | Sync currently exports first and imports second at `crates/cli/src/cli/commands/git_projection_io.rs:548-578`. | Reverse the semantic order: stage/import, update native mappings, then resolve and materialize exactly once. No raw imported ref may remain after the final gate. |
| Projection push | `GitProjection::push` exports and then sends the destination at `crates/git-projection/src/git_core.rs:962-1035`; path export/copy is at `crates/git-projection/src/git_core.rs:1056-1160`, and network push is at `crates/git-projection/src/git_core.rs:4605-4673`. | All three consume the same plan and the same destination ownership/delete-set. No second ref enumeration after resolution. |
| Authoritative Git-overlay push | The CLI passes the raw overlay repository to `push_authoritative_git_refs` at `crates/cli/src/cli/commands/remote/mod.rs:141-190`. That function enumerates and filters raw Git refs directly at `crates/git-projection/src/git_core.rs:4748-4778`. | Import/stage the overlay view, resolve from native identities, and push the plan. Remove this raw source-to-remote publication bypass. |
| Raw tag carry-through | `mirror_checkout_tags_for_push` copies and claims raw checkout tags after export at `crates/git-projection/src/git_core.rs:1404-1445`. | A raw tag is managed only after it resolves to an exact served native target. Never append raw tags after planning. |
| Ingest staging | `stage_ingest_source_in_mirror` applies source refs and marks them managed at `crates/git-projection/src/git_core.rs:1519-1548`. | Stage in a non-published ingress namespace. Only plan output enters the managed publication manifest. |
| Local overlay write-through | Local checkout changes write refs/notes at `crates/git-projection/src/git_core.rs:1760-1916`. | This remains the repository owner's local view, not a public serve. Every outward push from it still passes through the plan; local writes never establish public servedness. |

The current exporter already has pieces of a plan, including desired projection
and served sets (`crates/git-projection/src/git_export.rs:753-786`) and separate
branch/tag reconciliation (`crates/git-projection/src/git_export.rs:797-987`).
Those pieces must become consumers of the one resolution rather than independent
gates. Direct visibility is currently checked inside `export_state`
(`crates/git-projection/src/git_export.rs:102-138`), entered from scoped export at
`crates/git-projection/src/git_export.rs:346-380`, while the topological mint walk
adds its own parent-mapping conditions
(`crates/git-projection/src/git_export.rs:596-648`). The present frontier walk
then stops at the first mapped state on each path and chooses one lowest ID; it
neither returns the maximal antichain nor publishes all roots
(`crates/git-projection/src/git_export.rs:1312-1348`). None is the single gate in
§5.1.

### 5.3 Surface projection rules

**Threads/branches.** Placement is stateful so a mutable antichain cannot move a
branch sideways. For frontier `F_i` and the audience/thread publication cursor:

```text
if no cursor exists:
    primary = min(F_i, by raw StateId bytes), or absent when F_i is empty
    persist primary as the initial cursor before publication
else if cursor is no longer served:
    retract the branch; do not choose a replacement
else:
    own = { f in F_i | f is a descendant of cursor }
    common = { served v | v is a descendant of cursor
                          AND v is an ancestor of every f in own }
    maxima = maximal elements of common
    primary = the sole element of maxima, if there is one; otherwise cursor
    persist a superseding cursor only when primary advances
```

When `own` is empty, `common` is defined as `{cursor}`, so a force-moved logical
root cannot make the public branch jump to another line. The `common` intersection
advances through the longest unambiguous shared line before a fork; after a served
merge reunifies the paths it advances to that merge. If criss-cross topology
leaves multiple incomparable maximal common descendants, the branch holds rather
than applying a tie-break. Every move after the initial anchor is therefore a
descendant move. Every member of `F_i` not already reachable as an ancestor of
`primary` is named synthetically.

The one initial tie-break is the frontier member with the least raw `StateId`.
That selection is persisted and replicated, never recomputed by a new host or a
new destination. A served merge that dominates the frontier advances the branch
and makes redundant synthetic refs deletable.

There is no durable cross-host publication cursor in the current tree. A local
destination's managed-ref manifest is not a replicated placement fact. Therefore
the cursor is a prerequisite of stable multi-host branch placement, not state that
`resolve_frontier` may infer from whichever destination it happens to inspect.
Its minimal semantic key is `(repository, audience, logical thread)` and its value
is the anchored/last advertised `StateId` plus a supersede/generation token.
Adding the storage and propagation substrate is a proposed follow-up in §11.

If a current restrictive `Set` makes the cursor state unserved, the plan deletes
the managed branch and its synthetic refs rather than rewinding the branch to an
ancestor or jumping sideways. It may be recreated only after an explicit policy
permits the state again. This cannot revoke bytes already fetched; it is only a
safe forward-publication response to behavior the current substrate permits.

**Synthetic roots.** Every maximal frontier member not reachable from the primary
branch gets:

```text
refs/heddle/frontier/<percent-encoded-thread>/<full-state-id>
```

`<full-state-id>` is the canonical full `StateId`, not `short()`, `Display`, or a
bare `ChangeId`. Percent encoding is byte-oriented UTF-8 with uppercase hex and
encodes `%`, `/`, and every byte outside Git's conservative ref-component set;
encoding then decoding must round-trip the exact thread name. Synthetic refs are
Heddle-managed and destination deletion applies when the state becomes reachable
from the primary branch or ceases to be served.

This namespace needs an explicit type/reservation. Current Git ref
classification recognizes only branch/tag/note as content namespaces
(`crates/repo/src/git_ref_name.rs:11-20,119-145`), current ref validation does not
reserve `refs/heddle/*` (`crates/refs/src/refs/name.rs:11-36`), and
`ThreadName::new` itself does not enforce such a reservation
(`crates/object-model/src/object/identifiers.rs:15-25`). Raw receive/import paths
must reject user writes to `refs/heddle/*`; Heddle-aware fetch needs an explicit
refspec and a type-distinct destination, not a user thread.

**Markers/tags.** A tag has identity semantics, not moving-frontier semantics. If
its exact native `StateId` is served, publish it at that exact mapped OID. If not,
withhold/delete the managed tag. Never lag a tag to a parent.

**Notes.** Notes are derived only after branch, tag, and synthetic roots are
known. Their complete rebuild is §6.

**HEAD.** For filesystem/bare exports, write a symref only to a branch present in
the same plan. Prefer the repository's selected default thread; otherwise use the
lexicographically least published branch name. With no served branch, write an
unborn `HEAD` and publish no detached raw OID. The current helper that writes a
HEAD symref is at `crates/git-projection/src/git_core.rs:3389-3395`; hosted default-
branch control must consume the same plan rather than infer from raw refs.

## 6. `refs/notes/heddle` rebuild

### 6.1 Decision: full logical rebuild, parentless every time

The current notes writer appends a commit to the previous notes head
(`crates/git-projection/src/git_notes.rs:156-180`). Removing a note writes another
descendant commit (`crates/git-projection/src/git_notes.rs:183-218`). That hides
the note from the new tip tree but leaves its blob/tree reachable by walking notes
history, so it is not an embargo-safe publication algorithm.

For each invalidated publication generation:

1. Start from an empty notes tree. Do not use the old notes commit as a parent and
   do not copy its tree.
2. Take the plan's desired branch, exact-tag, and synthetic commit roots. Walk
   their Git commit ancestry and retain only OIDs mapped to `served == true`.
3. Construct the canonical `HeddleNote` payload for each retained mapped
   `StateId`. Sort entries by the annotated Git OID's raw bytes before building
   the standard fan-out notes tree.
4. Build note blobs and trees deterministically. Build one parentless commit with
   fixed identity `Heddle Frontier <frontier@heddle.invalid>`, timestamp `0 +0000`,
   and message `heddle served notes v1`. Git object format is part of the
   generation key.
5. If there are no desired notes, the plan deletes `refs/notes/heddle`. Otherwise
   it sets the ref to the new root commit OID.
6. Compare-and-swap the previously planned notes OID. The expected-old match
   authorizes a non-fast-forward replacement of this Heddle-managed history; it
   is not a blind force. A destination change not made by the same managed
   generation aborts publication; never overwrite unrecognized/foreign ownership.

The current reader lists notes from the current tip tree
(`crates/git-projection/src/git_notes.rs:249-263`), so a parentless canonical tip
is compatible with reads. Old notes objects may remain unreachable in a local
object database; the publication guarantee is that no old notes parent or hidden
note object is reachable from the newly published ref and no destination transfer
roots such objects independently.

### 6.2 Invalidations, caching, and ref ordering

A rebuild is invalidated by any change to:

- a selected logical raw target or publication cursor;
- any visited state or effective visibility-head hash;
- a StateId-to-Git-OID mapping or Git object format;
- audience/scope;
- the note payload/schema or deterministic commit recipe.

An exact generation-digest hit may reuse the prior canonical notes OID. This is
object reuse, not incremental history. A miss always rebuilds from an empty tree.

Materialize all objects before refs. In the internal mirror, apply the plan's
heads, tags, synthetics, notes ref, and deletions in one compare-and-swap ref
transaction when available. For a remote lacking atomic multi-ref update, every
individual target is already served; use per-ref expected-old CAS, replace/delete
the notes ref before adding newly exposed roots, publish branch/tag/synthetic refs
next, and update hosted/default `HEAD` last. Any CAS failure aborts and recomputes
the complete plan. The destination ownership manifest is advanced only after all
planned ref outcomes are confirmed.

## 7. Consistency and failure semantics

`resolve_frontier` is pure over an identified snapshot. Publication is a validate-
then-apply protocol:

1. Acquire/read a coherent native snapshot and confirmed visibility generation.
2. Run due-schedule materialization (§8), if this process is the authority.
3. Resolve the complete frontier and publication plan from the post-promotion
   generation.
4. Mint/copy only the plan's served objects and build the canonical notes root.
5. Revalidate the generation and expected destination refs.
6. Apply refs with CAS and record the managed set.

If authoritative visibility replication cannot be confirmed, a replica either
continues serving its last fully materialized immutable plan or blocks the
publication. It must not reinterpret a missing sidecar as public and must not
re-resolve from a partial fact set. Public-by-absence remains valid only inside
the native resolver when the confirmed generation proves the authoritative
record set is complete. The current Git mirror plus exported-ref manifest can
embody the last plan for Git; #318 needs the equivalent confirmed serve
generation.

The current code's direct note writers
(`crates/git-projection/src/git_export.rs:596-730`) and raw ref collection
(`crates/git-projection/src/git_core.rs:3113-3132`) are exactly the APIs that a
conformance test must prevent publication code from calling after the plan
boundary.

## 8. Scheduled `StateVisibilityPromote`

### 8.1 Trigger and authority

A schedule is eligible when the **effective head record**, not any superseded
record, satisfies all of:

- `tier != Public`;
- `embargo_until = Some(t)`;
- the authority's captured cutoff `now >= t`.

The repository's hosted control plane designates one promotion authority/leader
per repository. In a local-only repository, the process about to perform a public
export/push acts as authority while holding the repository write lock. Periodic
workers may eagerly run the same barrier, but the required trigger is the first
public publication attempt at or after the deadline. No publication means there
is no requirement to wake exactly at `t`.

The observable transition is therefore **not earlier than** `embargo_until` and
may be later: it occurs only after the next authority barrier has committed the
fact, propagated it, received the required acknowledgements, and published a plan
from that confirmed generation. `scheduled_for` records the requested instant;
the signed record's `declared_at` records actual materialization.

Secondary serve/export hosts never fire schedules. They consume persisted signed
facts from the authority. Before using a newly public result, the authority
propagates the new `StateVisibilityBlob` and corresponding operation record to
every serving host in scope, advances the visibility-generation manifest, and
receives acknowledgement of the exact record hash and manifest token. A host
without that acknowledgement serves its last confirmed plan or blocks.

Promotion changes only the scheduled state's direct tier. It does not silently
promote ancestors: `StateVisibility` is explicitly per-state. The state becomes
served only when §4 also finds every parent served. A schedule-creation UI should
warn when ancestor visibility/schedules mean the requested state will remain
withheld after its own deadline.

### 8.2 Deterministic batch ordering

At the start of a public publication, the authority captures one UTC cutoff and,
under the repository write lock, enumerates current effective scheduled heads due
at that cutoff. The repository already has a full sidecar enumeration primitive
at `crates/repo/src/repository_state_visibility.rs:565-593`; use it for the first
implementation and add an index only if measurement justifies one. Sort candidates
by:

```text
(embargo_until UTC instant, StateId raw bytes, effective-head ContentHash bytes)
```

For each candidate in order:

1. Re-read its effective head under the same lock.
2. If the head hash is no longer the selected hash, the old schedule was
   superseded/cancelled; skip it. A new due head will be found by the next barrier
   or by restarting enumeration.
3. Create a signed `StateVisibility { state, tier: Public, embargo_until: None,
   declarer: authority, declared_at: materialization_time, supersedes:
   Some(selected_head), ... }`.
4. Persist it together with its `StateVisibilityPromote` operation through the
   combined visibility commit primitive.

A schedule inserted concurrently after cutoff waits for the next barrier. That
may withhold longer but cannot disclose early. If any candidate fails to persist,
the public plan is not produced. Already committed earlier candidates remain valid
monotonic facts, and retry resumes idempotently.

The repository lock totally orders manual visibility mutations with the whole due
batch. A manual `Set`/`Promote` that commits first changes the effective head, so
the stale scheduled candidate is skipped. If the scheduled batch commits first,
the later manual command observes the new Public head; any later narrowing is then
governed by the unresolved post-serve policy in §10 rather than being reordered
ahead of the schedule.

### 8.3 Concrete `OpRecord` shape

The variant already exists with `state`, `superseded`, `record_id`, `tier`, and
full before/after sidecar images
(`crates/schema/src/op_record/types.rs:348-368`). Keep those fields and append one
optional tail field:

```rust
StateVisibilityPromote {
    state: StateId,
    superseded: ContentHash,
    record_id: ContentHash,
    tier: VisibilityTier,
    #[serde(default)]
    prior_sidecar: Option<Vec<u8>>,
    #[serde(default)]
    new_sidecar: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scheduled_for: Option<DateTime<Utc>>,
}
```

Semantics:

- `scheduled_for = None` means an explicit/manual promotion and preserves current
  behavior.
- `scheduled_for = Some(t)` means the authority materialized the schedule whose
  superseded effective record contained exactly `embargo_until = Some(t)`.
- For `Some`, `tier` MUST be `Public`; `record_id` MUST hash a new signed record
  with `supersedes == Some(superseded)` and `embargo_until == None`.
- `declared_at` and `declarer` are already authenticated inside the record named
  by `record_id` and captured in `new_sidecar`; duplicating them in `OpRecord`
  would create two authorities for the same fact.
- The before/after images remain for crash recovery and audit, but an automatic
  scheduled promotion is repository-global and MUST be excluded from ordinary
  checkout-lane `undo`. Manual promotion keeps the current undo contract unless
  the owner resolves the broader monotonicity question differently.

The new field is optional so existing manual records can remain byte-identical.
The current codec freezes the six-field bytes of this variant
(`crates/schema/src/op_record/codec.rs:642-666`); implementation must use both
`default` and `skip_serializing_if` and add a scheduled golden fixture/schema
ledger entry. Adding a field to an rmp enum variant is a variant reshape under
the repository schema policy
(`docs/spikes/heddle-451-schema-versioning-policy.md:104-115`), so implementation
MUST bump/select a new OpRecord payload schema before emitting
`scheduled_for: Some`.
The byte-identical `None` encoding eases migration but does not waive that bump.

### 8.4 Idempotency, replay, and races

For each selected head, derive the mutation transaction key without wall-clock or
random input:

```text
"state-visibility-scheduled-public-v1/" +
    full(StateId) + "/" + full(superseded ContentHash)
```

The atomic mutation substrate already supports unbounded exact-once lookup by a
stable `transaction_id` and returns the prior committed records on replay
(`crates/repo/src/atomic/tx.rs:335-365`). The scheduler uses that stable key; it
does not call `OperationId::new`, which creates a new UUIDv7 on each attempt
(`crates/object-model/src/object/operation_id.rs:15-38`).

The scheduled path is a dedicated
`commit_scheduled_visibility_promotion(expected_head, scheduled_for,
transaction_id)` wrapper around the visibility mutation and atomic transaction
substrates. Under the repository lock it first looks up `transaction_id`; an
already committed key restores/returns the committed `new_sidecar` and operation
without stamping or signing again. Only an uncommitted key checks
`expected_head`, stamps/signs the new record, registers restoration of the exact
`prior_sidecar`, and reaches the exact-once oplog commit point.

The current combined primitive rolls the sidecar back when an oplog append returns
an error (`crates/repo/src/repository_state_visibility.rs:388-415`), but a process
death between those writes still needs deterministic recovery. Before any
publication resolution, the wrapper reconciles a retained Public record that has
no matching committed visibility operation: while holding the lock, restore the
blob to the selected `superseded` head and retry the stable transaction. Such an
orphan is never propagated and never accepted by `resolve_frontier`; conversely,
a committed operation whose sidecar is missing restores its recorded
`new_sidecar`. This makes the sidecar plus committed operation the publication
fact, not either half alone.

Replay rules are:

- If the transaction key is already committed, return the original record and do
  not create a new signature, timestamp, sidecar entry, or oplog entry.
- If the effective head still equals `superseded`, exactly one contender may
  commit. Others replay that transaction or observe the changed head and skip.
- If the head differs and the key was never committed, the selected schedule is
  stale/cancelled. Do not promote the replacement record unless it independently
  qualifies.
- Receiving the exact signed record again is a content-hash deduplication no-op;
  the current sidecar put already detects an existing record by content hash
  (`crates/repo/src/repository_state_visibility.rs:217-229`).
- A replayed oplog batch retains the same per-state isolation key; visibility ops
  already declare that key at `crates/schema/src/op_record/types.rs:543-550`.

Only after every due mutation for the cutoff is committed and its required
replication acknowledgement is present does `resolve_frontier` compute the public
plan. Thus “promotion persisted” happens before “promotion affects any ref,” and
an authority crash at every boundary is replay-safe.

## 9. What is deliberately not re-derived per surface

The following decisions exist only in the native resolution/publication plan:

- effective audience visibility;
- downward-closed servedness;
- missing-object and shallow-boundary policy;
- maximal frontier and minimal hidden cut;
- primary placement/cursor interpretation;
- exact-tag withholding;
- note eligibility and canonical notes root;
- synthetic-ref names and deletion;
- destination managed ownership/delete-set;
- `HEAD` eligibility;
- the snapshot/generation token.

Surface adapters translate the plan to local mirror ref transactions, filesystem
refs, or network ref updates. They may not filter raw refs “one more time,” add
checkout tags afterward, backfill notes from the mapping, or infer public by
sidecar absence outside the native resolver's confirmed-complete generation.

## 10. Open owner questions

1. **Post-serve narrowing policy.** The accepted product text says a served state
   never re-embargoes, but current `visibility set` permits narrowing and the tree
   has no durable per-audience served ledger. Decision needed: add a ledger and
   reject narrowing after first serve, or explicitly retain the safe managed-ref
   retraction semantics in §5.3 while acknowledging that bytes are irrevocable.
   The resolver must support retraction until that decision is implemented.
2. **Hosted authority ownership.** Which deployed component owns the repository-
   promotion lease and visibility-generation acknowledgements? The semantics do
   not depend on the component choice, but #319 cannot claim multi-host scheduled
   promotion complete without one named authority and propagation path.
3. **Publication cursor home.** Stable initial antichain placement requires a
   replicated cursor that is absent today. Decide whether it is a native signed
   object/oplog fact shared by Git and wire (preferred) or a hosted control-plane
   record. Per-destination Git metadata alone is insufficient.

## 11. Proposed follow-ups

Do not file these until the orchestrator confirms scope:

- **Shared native frontier resolver:** add the §4 union-DAG algorithm, explicit
  shallow proof input, generation token, and linear/multi-root/octopus/criss-cross
  conformance vectors shared by #318 and #319.
- **Sealed Git publication plan:** route export, import→publish, sync, projection
  push, authoritative-overlay push, raw tag carry-through, notes, `HEAD`, and
  destination deletes through one plan; make scoped/full results equivalent.
- **Replicated publication cursor:** add the per-audience/thread durable anchor and
  forward-only placement fact required for host- and destination-independent
  antichain placement.
- **Synthetic frontier namespace:** reserve/type
  `refs/heddle/frontier/*`, use full `StateId`, add Heddle-aware fetch refspecs, and
  reject raw receive/import writes to `refs/heddle/*`.
- **Canonical notes rebuild:** implement deterministic parentless notes commits,
  CAS replacement/deletion, and reachability tests proving no unserved note object
  is reachable from the published notes ref.
- **Scheduled promotion substrate:** retain superseding `Public` records, add due-
  schedule scanning over the existing repository enumeration (and index only if
  measured necessary), stable exact-once transactions, the
  `scheduled_for` OpRecord tail/schema fixture, and scheduled-promotion undo
  classification.
- **Propagate-before-publish integration:** designate the hosted authority, sync
  and acknowledge visibility generations to all serve/export hosts, and keep or
  block on the last confirmed plan when facts are incomplete.
- **Publication boundary audit:** add compile-time/module-boundary restrictions and
  tests that no raw ref update reaches a publisher; cover destination retraction,
  foreign-ref survival, scoped/full equality, raw overlay push, raw tags, notes
  history, and `HEAD`.

## 12. Implementation acceptance checks for #319

An implementation is not complete unless tests demonstrate:

- a hidden linear state lags a branch to the last served ancestor without minting
  the hidden state;
- a hidden merge publishes every maximal served parent line, including octopus
  and criss-cross shapes, and a later served merge collapses the synthetic roots;
- directly visible descendants of a hidden ancestor remain absent;
- an exact tag is withheld rather than retargeted;
- a notes rebuild has no parent and no hidden state's note blob/tree is reachable
  from `refs/notes/heddle`;
- import, export, sync, local path export, network push, and authoritative overlay
  push produce the same managed ref set for the same audience/generation;
- a retracted Heddle-managed ref is deleted at an existing destination while a
  foreign ref survives;
- a missing non-shallow parent fails closed;
- a due schedule creates one signed persisted Public record and one oplog record
  under crash/retry/race, and a secondary host never fires it from local time;
- publication cannot observe the new frontier before promotion propagation is
  acknowledged.
