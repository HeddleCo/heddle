# Commit-level visibility tiers within a thread — design spike

> **Status:** spike (doc-only). Design before impl. This document commits to a
> *path*; the implementation lands as the follow-up issues listed in §10. It
> does **not** implement the feature.
>
> Tracks HeddleCo/heddle#266. External motivation: the "VCS for the agent era"
> argument that *partial openness* — ship a security fix without publishing the
> exploit, hide an in-flight PR, expose a draft to a named reviewer audience —
> is a missing primitive. Internal motivation: heddle's self-assessment placed
> the gap at "commit-level granularity" of visibility.
>
> Every concrete artifact cited below (`crates/...:line`, type, field, RPC,
> flag) was grepped against the tree at spike time. Where a capability is built
> but not yet wired, or planned-not-shipped, it is labeled per the AGENTS.md
> truth convention (`shipped` / `foundation in place` / `planned`).

---

## 1. Problem framing

Heddle has a **thread-level** openness boundary today: a thread is either
heddle-native (reachable only through the wire protocol with a grant) or
mirrored to a public Git branch via the bridge. What it lacks is a
**within-thread, per-commit** boundary. Three scenarios are blocked by that gap:

1. **Embargoed security fix.** A public thread (mirrored to `refs/heads/main`)
   has commit `N` describing an exploit and commit `N+1` fixing it. Today both
   publish together or both stay private. Needed: ship the fix publicly while the
   exploit detail stays embargoed until a disclosure date. (Soundness constrains
   the topology: a fix can be published ahead of disclosure only when the
   embargoed detail is **not an ancestor** of it — the public mirror is
   downward-closed under ancestry, §5.0. When the exploit commit precedes the fix
   on one line, both wait for disclosure on the public mirror while authorized
   pullers get the fix immediately; worked through in §7.1.)

2. **In-flight PR / draft review.** A reviewer can see a proposed commit; the
   public cannot. Today a PR is either a fully private thread (invisible without
   a grant) or public from first push. Needed: a thread that is "open for
   review" to a specific reviewer audience while hidden from the public.

3. **Monorepo with private sub-packages.** Within one public thread, some paths'
   commits are visible only to a private-package audience. **Out of scope here**
   — per the issue, this is a *per-path* axis, not a per-commit one, and is
   handled separately. We note it only to bound this spike to the per-commit
   axis.

The deliverable is a per-commit (per-*state*, in heddle terms) visibility tier
with three audiences — **public / reviewer-scoped / private** — enforced when
heddle serves refs and objects, plus a "promote" transition (embargoed →
public) that does not rewrite history.

### 1.1 The one distinction that drives the whole design

There are two fundamentally different strengths of "hidden", and heddle already
ships one of them (redaction) but not the other:

- **Cooperative render-hide (soft).** The bytes still travel to the peer; a
  sidecar record instructs the reader to render a stub instead of the content.
  This is exactly how `Redaction` works today (§3.3): the blob bytes sync over
  the wire alongside the redaction record (`crates/proto/src/object_graph.rs:346`),
  and each read chokepoint substitutes a stub. Soft-hiding is only as strong as
  the recipient's cooperation — anyone holding the bytes can ignore the record.

- **Serve-withhold (hard).** The server never sends the protected object to an
  under-tier caller. The bytes do not leave the host. This is the *only* sound
  enforcement for a real embargo against an adversarial puller, because the
  recipient controls its own materialize path.

A true embargoed security fix demands **hard** enforcement: if the exploit
commit's tree reaches a public clone, the embargo is already broken, no matter
what any record says. An in-flight-PR hide is comfortable with **soft**
enforcement on infrastructure you control, but wants **hard** on a public host.
The recommended design (§5) therefore makes the **serve-side withhold the source
of truth for every public-tier projection**: both the wire serve and the
Git-bridge export emit *absence*, never a partial view of an embargoed commit
(the governing invariant, §5.0). The redaction render-stub is reused **only on
the operator's own local checkout** — where the holder already possesses the
bytes and the stub is a working-tree courtesy, not a security boundary — and
never as a public-mirror exposure path.

---

## 2. The current model this builds on (grounded)

### 2.1 State = the commit

A commit in heddle is a `State`: an immutable, content-addressed snapshot
(`crates/objects/src/object/state_core.rs:201`).

```rust
pub struct State {
    pub change_id: ChangeId,            // stable 128-bit logical id
    content_hash: Option<ContentHash>,  // cached BLAKE3 over the bytes (skip-serde)
    pub tree: ContentHash,              // root tree
    pub parents: Vec<ChangeId>,         // DAG edges, by ChangeId (not content hash)
    pub attribution: Attribution,
    pub intent: Option<String>,
    pub confidence: Option<f32>,
    pub created_at: DateTime<Utc>,
    pub verification: Option<Verification>,
    pub signature: Option<StateSignature>,
    pub status: Status,                 // Draft | Published (state_core.rs:16)
    // --- tail-only optional fields below; new fields go here, never above. ---
    pub provenance: Option<ContentHash>,
    pub logical_change_id: Option<ChangeId>,
    pub context: Option<ContentHash>,
    pub authored_at: Option<DateTime<Utc>>,
    pub risk_signals: Option<ContentHash>,
    pub review_signatures: Option<ContentHash>,
    pub discussions: Option<ContentHash>,
    pub structured_conflicts: Option<ContentHash>,
}
```

Two properties matter for this design:

- **Immutability + content addressing.** A `State`'s `content_hash` is the
  BLAKE3 of its serialized bytes, and the doc-comment invariant
  (`state_core.rs:185-200`) forbids mutating it: new optional fields append at
  the tail with `#[serde(default)]`; nothing in the middle moves. Anything that
  needs to *change* about a state after the fact — a redaction, a review
  signature, a discussion — lives in a **sidecar referenced by content hash**,
  not in the state body. Visibility must follow the same rule.
- **Parents by `ChangeId`, not content hash.** `parents: Vec<ChangeId>`
  (`state_core.rs:207`). A child references its parent by the stable logical id,
  so withholding or stubbing a parent's *content* does not break the child's DAG
  edge — the edge is a 16-byte id, independent of whether the parent's tree is
  served. This is what makes "hide `N`, show `N+1`" representable at all (§7.1).

`Status` is `Draft | Published` (`state_core.rs:16-20`) — a *lifecycle* flag,
not an access tier. It is the wrong axis to overload for visibility (a published
commit can still be embargoed; a draft can be reviewer-visible).

### 2.2 Threads point at states by ref

A thread is a `ThreadRecord` (`crates/repo/src/thread_model.rs:196`) whose tip
is a `ChangeId`. The ref binding is `RefUpdate::Thread { name, expected, new:
Option<ChangeId> }` (`crates/refs/src/refs/types.rs:16-17`). There is **no
access-tier field on a thread or a state today** — grep for a `visibility` field
on either struct comes back with only the annotation/discussion uses below, none
on `State` or `ThreadRecord`.

**Naming hazard.** "Visibility" is already a word in the thread vocabulary, but
it means *workspace/checkout presence*, not *who-can-read*:
`thread_human_visibility` (`crates/cli/src/cli/commands/thread.rs:1137`) returns
`"imported Git branch"`, `"no dedicated checkout"`, or a workspace-mode label;
the proto `ThreadSummary.visibility` field
(`crates/grpc/proto/heddle/v1/service.proto:1801`) carries that same
materialization status. To avoid collision, this design names the new concept an
**audience tier** (aligning with the existing `AudienceTier` enum, §2.4), never
a bare "visibility" on threads/states in user-facing surfaces.

The public-vs-private *thread* distinction the issue references is, in code,
conceptual: `OperationScope` is only `Git | Heddle`
(`crates/repo/src/repository.rs:156-161`) — git-overlay vs heddle-native — and
whether a thread is "public" is decided by whether the bridge exports it to a Git
ref (§2.5), not by any stored access flag.

### 2.3 The sidecar precedent: `Redaction`

`Redaction` is the closest existing primitive and the template for this design
(`crates/objects/src/object/redaction.rs:29`):

```rust
pub struct Redaction {
    pub redacted_blob: ContentHash,
    pub state: ChangeId,                 // scoped to a (blob, state, path) triple
    pub path: String,
    pub reason: String,
    pub redactor: Principal,
    pub redacted_at: DateTime<Utc>,
    pub signature: Option<StateSignature>,  // signs canonical_signing_payload (redaction.rs:67)
    pub purged_at: Option<DateTime<Utc>>,
    pub supersedes: Option<ContentHash>,    // supersede chain for refinement
}
```

It is stored as a per-blob sidecar `RedactionsBlob { format_version, redactions }`
(`redaction.rs:133`), one `rmp-serde` file per blob hash under a redactions
directory (`crates/repo/src/repository_redaction.rs:490-496`,
`crates/objects/src/store/fs/fs_paths.rs:46-50`). It is **additive** — declaring
a redaction never mutates the state or the blob; the bytes stay on disk until an
explicit `purge`.

> **Important correction to the issue's framing.** The issue lists
> `OpRecord::Redact` / `OpRecord::Purge` as primitives for "commit-level
> removal." They are **blob-level**, not commit-level: `Redact` carries
> `{ redaction_id, blob, state, path }` (`crates/oplog/src/oplog/oplog_types.rs:109`)
> and is scoped to a `(blob, state, path)` triple (`redaction.rs:32-37`). They
> are the right *pattern* to imitate (additive sidecar, signed, supersede chain,
> resolve-layer stub) but the wrong *granularity* to reuse directly — commit
> visibility needs a state-keyed sidecar, not a blob-keyed one.

### 2.4 The tier vocabulary already exists (foundation in place)

Heddle already has a complete, unit-tested *vocabulary* and *filter* for
audience tiers — currently applied to annotations and discussions, not yet to
commits or any serve path:

- `AnnotationVisibility` (`crates/objects/src/object/state_context.rs:52`):
  `Public` (`#[default]`), `Internal`, `TeamScoped { team_id }`,
  `Restricted { scope_label }`. Used as a real field on `Annotation`
  (`state_context.rs:36`) and on `Discussion`
  (`crates/objects/src/object/discussion.rs:87`).
- `AudienceTier` (`crates/repo/src/visibility.rs:33`): `Internal`, `Public`,
  `Team(String)`, `Restricted(String)` — the *reader's* tier. It parses from the
  string grammar `internal | public | team:NAME | restricted:LABEL`
  (`visibility.rs:60-87`).
- The mapping (who-sees-what) is a single source-of-truth function `visible()`
  (`visibility.rs:148`) with the table documented at `visibility.rs:14-19`, and
  two filters `filter_for_audience` / `filter_for_audience_with_drops`
  (`visibility.rs:110, 123`) — the latter reports per-scope drop counts so a
  surface can show "N hidden by your audience tier".
- Defaults resolve through a 4-tier chain
  (`crates/repo/src/namespace_policy.rs:1-12`, fn at
  `namespace_policy.rs:68`): explicit `--visibility` → `[namespace.<name>]
  default_visibility` → repo-wide default → hard-coded `Internal` fallback (the
  safe "we don't know who should see this" choice).

> **Grounding caveat (do not overstate this).** The `visibility.rs` module
> doc-comment asserts "every annotation read path flows through one of
> `filter_for_audience`", but as of this spike `filter_for_audience` /
> `filter_for_audience_with_drops` have **no callers outside their own module
> and tests** (grep: no external callers), and `resolve_default_visibility` is
> exercised only by its own tests. The tier *machinery* is built and tested;
> it is **not yet wired into any live serve/render path**. Likewise the
> `bridge git export --audience` / `--notes` flags referenced in code comments
> (`crates/cli/src/bridge/git_export.rs:45-46`) are **planned** — the shipped
> export subcommand takes only `--destination`
> (`crates/cli/src/cli/cli_args/commands_bridge.rs:76-81`). So the tier model is
> excellent *substrate to build on*, but commit visibility is the feature that
> would give it its first real caller, and it must wire the filter end-to-end
> itself rather than assume an existing pipe.

### 2.5 The resolve / serve chokepoints

There are three places where heddle turns a stored object into something a
consumer sees. Redaction already enforces at the first two; commit visibility
must enforce at all three:

1. **Local materialize** — `materialize_blob`
   (`crates/repo/src/repository_materialization.rs:575`) short-circuits to a
   stub when `redaction_stub_for_blob` returns one (`:591`).
2. **Git bridge export** — `export_tree`
   (`crates/cli/src/bridge/git_export.rs:110-164`) substitutes a stub at the
   single chokepoint where a blob crosses into a downstream Git remote (`:127`).
   States are selected by walking `reachable_states` from a thread tip and minting
   one Git commit per state (`git_export.rs:182-246`).
3. **Wire protocol serve** — `RepoSyncService`
   (`crates/grpc/proto/heddle/v1/service.proto:8-13`):
   `ListRefs` → `RefEntry { name, change_id, is_thread }` (`:208-222`),
   `UpdateRef` (`:224`), and the streaming `Push` / `Pull`. The object set sent
   over the wire is computed by the planner in `crates/proto/src/object_graph.rs`,
   which already has an *exclusion* pass (`collect_excluded`, `object_graph.rs:360`)
   for shallow/exclude negotiation, and propagates redaction records alongside
   blobs (`emit_redaction_plan`, `object_graph.rs:346`). **The authoritative
   server-side handlers live in the closed `weft` repo**, not in this OSS
   workspace — the workspace has the client stubs (`crates/client/src/grpc_hosted/`)
   and the auth context (`Permission` at `crates/proto/src/message_auth.rs:10`,
   `TokenScope` at `crates/proto/src/auth_token.rs:16`).

### 2.6 The grant model that supplies the caller's tier

Authorization today is **repo/namespace-level**, not thread/commit-level. The
role substrate (`Reader < Developer < Maintainer < Admin < Owner`) lives in weft
and is documented in `docs/spikes/contribution-grant-flows.md` §1. That spike
already proposes a `GetMyEffectiveRole` RPC over a target resource
(`contribution-grant-flows.md` §3.2, §4) as the UX layer over the same
substrate. Commit visibility needs a mapping from "caller's effective role/grant
on this thread (as the target resource)" → `AudienceTier`; that effective-role
RPC is the natural place the mapping is produced (§9, open question O2).

---

## 3. The CLI surface that exists (the ergonomics baseline)

The recommended CLI (§8) mirrors the **redaction verb family**, which is the
right-sized precedent (`crates/cli/src/cli/cli_args/commands_redact.rs`):

```
heddle redact apply <state> --path P --reason R [--all-states] [--sign-with PEM] [--sign-algo A]
heddle redact list
heddle redact show <id>
heddle redact trust add|list|remove        # fail-closed wire-trust list for signed redactions
heddle purge  apply <state> --path P --force
heddle purge  list
```

Key ergonomic properties to carry forward: one verb family, a signed
declaration, a supersede chain for refinement, an audited `OpRecord` per action
(`Redact`/`Purge` at `oplog_types.rs:109,123`), and a **fail-closed trust list**
(`RedactTrustCommands`, `commands_redact.rs:41`) governing which peers' signed
records are accepted over the wire.

---

## 4. Candidate designs

### Design A — visibility field inside `State`

Add `audience: StateAudience` to the `State` tail (`state_core.rs:215`).

- **Pros:** trivial to model; travels with the state automatically; no new
  object type or sidecar store.
- **Cons (disqualifying):** changing a tier (the *promote* transition) changes
  the state's `content_hash`, which either violates the immutability invariant
  (`state_core.rs:185-200`) and invalidates `signature`, or forces minting a new
  state object on every visibility change — history churn for a metadata edit.
  And the tier cannot gate withholding from *outside* the object at all: it lives
  inside the very state you would need to withhold, which (per §5.0) is served
  whole or not served. Rejected.

### Design B — state-keyed sidecar (mirror `Redaction`) + serve filter — **recommended**

A `StateVisibility` record keyed by `ChangeId`, stored in a per-state sidecar
`StateVisibilityBlob`, enforced by **one downward-closed reachability gate** (§5.0,
§5.3) — the same hard withhold on every public-tier projection (the wire serve
*and* the Git-bridge export both emit *absence*, never a partial view). A
render-stub appears only on the operator's own local checkout, as a courtesy on
already-held bytes — never a public-mirror surface. Promotion is an additive
superseding record (or an `embargo_until` lapse), never a state mutation. Detailed
in §5–§8.

- **Pros:** preserves `State` immutability and signatures (the sidecar is
  outside the hashed bytes, exactly like `Redaction`, `review_signatures`,
  `discussions`); promotion is additive and audit-friendly; builds on the proven
  substrate — `AudienceTier`, the `visible()` predicate (extended with a `Private`
  arm, §5.2), the namespace-default resolution *pattern* (generalized over the
  tier enum, §8.1), and the signed / supersede / fail-closed-trust patterns; and
  reuses the redaction *stub renderer* for the operator-local checkout courtesy
  (§5.3). The serve-side gate is a genuine hard boundary — every public-tier
  projection emits absence (§5.0), never a partial view.
- **Caveat on integration points:** visibility does **not** drop in at the exact
  chokepoints redaction guards. Redaction's chokepoints are blob-keyed; a
  state-keyed tier predicate has to move one level up to the state walk (§5.3),
  and the wire filter needs a *new* reachability pass rather than the existing
  `collect_excluded` (§5.3). The patterns are reused; the precise call sites are
  not the same.
- **Cons:** a two-layer mental model (state + visibility sidecar); the *hard*
  serve gate must be implemented in the closed weft serve path (this OSS spike
  can specify the object, the records, the wire-plan reachability gate, and the
  operator-local stub, but the authoritative withhold lands in weft); time-based
  auto-promotion raised a wall-clock trust question — **resolved** by materializing
  the schedule into a persisted monotonic record before first serve (§5.4, O5), so
  visibility is never recomputed from a clock.

### Design C — tier-per-ref (separate threads)

Model each audience as its own thread/ref — `main` (public),
`main@review` (reviewer-scoped) — and gate each ref with the existing
repo/namespace grant model.

- **Pros:** no new per-commit object; reuses thread-level grants; the serve
  filter degenerates to "can you see this ref."
- **Cons (disqualifying for the core case):** this is exactly the status quo the
  issue calls out as the gap — "today this requires splitting into a separate
  private thread." It forces an embargoed commit `N` and its descendant fix `N+1`
  onto *divergent refs* that then need reconciliation, instead of keeping them on
  one thread with per-commit tiers. (The public-mirror outcome is the same
  truncation either way — per the invariant §5.0 the public mirror stops at the
  embargo boundary regardless — but Design B keeps a single coherent line that the
  authorized audience sees whole and that discloses forward, whereas Design C
  permanently forks history.) Rejected as the primary design; retained as the
  fallback that already works for coarse cases.

---

## 5. Recommended design (Design B), in detail

### 5.0 The governing invariant — the public mirror is downward-closed under ancestry

Everything below is subordinate to one rule. It is not a preference; it is the
**residue left after eliminating every partial-exposure alternative** (the
"ruled-out traps" table below). It is what makes an embargo sound *by
construction* rather than by a chokepoint that has to filter correctly on every
request:

> **Each audience's view of the mirror is downward-closed under ancestry: it
> contains a commit only when that commit *and its entire ancestry* are visible
> to that audience.** For the public tier specifically, an embargoed commit is
> **entirely absent** from the public mirror — no stub, no header, no partial
> `State`, no tree, no blobs — and so are *all of its descendants*, until the
> embargoed ancestor discloses. Disclosure is **forward-only** (r4): when the
> ancestor becomes public, it and its now-eligible descendants are published
> with their **true parent edges and OIDs intact**; nothing already-published is
> ever rewritten.

Three properties of the *real* machinery force this all-or-nothing shape — there
is simply no sound *partial* embargoed-commit view to serve:

1. **The wire serializes the whole `State`.** The sync planner emits an
   `ObjectType::State` object whose payload is `rmp_serde::to_vec_named(&state)`
   over the *entire* `State` (`crates/proto/src/object_graph.rs:84-90`; the
   plan-only variant keys the same object by `ChangeId` at `:160-163`, and the
   wire object-id for a state is its `ChangeId`,
   `crates/client/src/grpc_hosted/helpers.rs:109`). `State` carries `intent`,
   `attribution`, `confidence`, `created_at`, `verification`, and `signature`
   (`crates/objects/src/object/state_core.rs:202-214`) — not just
   id/parents/tree. There is **no header-only `State` form** on the wire, so
   "serve `N`'s header so the DAG stays walkable" leaks exactly the
   exploit-describing metadata the embargo exists to hide.
2. **Content-addressed OIDs.** A Git commit's OID is a pure function of its tree
   + parent OIDs (`export_state` → `new_commit_as(..., git_tree_oid,
   parent_oids)` → `commit.id`, `crates/cli/src/bridge/git_export.rs:84-93`), so
   no published commit's identity can change after the fact, and no parent edge
   can be re-pointed without changing the descendant's own OID.
3. **The public mirror is fast-forward-only.** `sync_track_to_branch` guards
   every branch update with `ensure_commit_update_fast_forward`
   (`crates/cli/src/bridge/git_sync.rs:134`, `git_core.rs:2446-2466`), which
   rejects any non-descendant tip (`NonFastForwardRef`) — there is no force path.
   So any "rewrite on disclosure" is *forbidden*, not merely discouraged.

The chokepoint cannot filter an embargoed object per-field per-audience (1), the
OID cannot change on disclosure (2), and the mirror cannot be rewritten (3). The
only design that survives all three is: **never put an embargoed commit, in any
form, into the public mirror until it discloses** — and, since a descendant's
identity depends on its ancestors' OIDs, hold its descendants too until it does.

#### 5.0.1 Ruled-out traps (the invariant is what remains after eliminating these)

Each row was proposed across review rounds r2–r5 and is unsound. Documenting them
is itself spike output: the invariant is precisely the residue after the
alternatives fall.

| Trap | What it proposed | Why it is unsound | Root cause |
|---|---|---|---|
| **Audience-scoped partial serialization** | the serve chokepoint hands a public puller a *filtered/sanitized* `State` (id/parents/tree only) | the wire emits one opaque `rmp_serde` blob of the whole `State` (`object_graph.rs:84`); the chokepoint serves it whole or not at all — it cannot strip `intent`/`attribution`/etc. per audience | all-or-nothing `State` serialization |
| **Header-only** | serve `N`'s "header" (id + parents + tree pointer) so a child's DAG edge resolves, withhold only the tree/blobs | there is no header-only `State`; the `ObjectType::State` payload **is** the full `State` incl. `intent`/`signature`/timestamps (`state_core.rs:202-214`) — the header *is* the leak (finding cid 3324616942) | all-or-nothing `State` serialization |
| **Stub-swap** | publish a stub-tree commit at `N`'s slot now, replace it with the real tree at disclosure | swapping the tree changes `N`'s OID → cascades a new OID to every descendant → a non-fast-forward rewrite of the published mirror, which the FF guard forbids (and the mapping-skip means the swap silently never happens anyway) | content-addressed OIDs + FF-only mirror |
| **Parent-edge rewrite** | re-parent the published descendant `N+1` onto `N-1` to "skip" embargoed `N` | `N+1`'s parent OID is an input to `N+1`'s own OID; re-pointing it changes `N+1`'s OID → if `N+1` was already published, a non-FF rewrite (violates r4 identity-stability) | content-addressed OIDs + FF-only mirror |

The sound replacement for the last two is **delayed publication**, not rewriting:
gate the descendant out of the public mirror until its embargoed ancestor
discloses (the invariant), then publish forward with true edges. r4 and r5 are
therefore coherent — neither ever changes a published OID.

### 5.1 Data model (new — all `planned`)

A new object in the `objects` crate, modeled field-for-field on `Redaction`:

```rust
// planned — crates/objects/src/object/state_visibility.rs
pub struct StateVisibility {
    pub state: ChangeId,                    // the commit this tier applies to
    pub tier: VisibilityTier,               // see 5.2
    pub embargo_until: Option<DateTime<Utc>>,  // auto-promote-to-public time, if any
    pub declarer: Principal,
    pub declared_at: DateTime<Utc>,
    pub signature: Option<StateSignature>,  // signs a canonical payload, like Redaction
    pub supersedes: Option<ContentHash>,    // promote = append a superseding record
}

pub struct StateVisibilityBlob {           // per-state sidecar, like RedactionsBlob
    pub format_version: u8,
    pub records: Vec<StateVisibility>,
}
```

Stored one `rmp-serde` file per state under a `visibility/` directory keyed by
`ChangeId`, mirroring the redactions store layout
(`repository_redaction.rs:490-496`, `fs_paths.rs:46-50`). The *effective* tier of
a state is the latest non-superseded record (mirroring
`RedactionsBlob::latest`, `redaction.rs:174`) — a pure function of the **persisted
records only**, never of wall-clock at read time. `embargo_until` is **not** a
serve-time predicate: it is an advisory *schedule* that the authoritative host
converts into a persisted superseding record when it fires (§5.4); the effective
tier is then read from that record, exactly like a manual `promote`. (This is what
keeps the tier monotonic under clock skew and across multiple serve hosts — the
durability rule in §5.4.) The state's **initial** record is written **at capture**,
not at first serve: the inherited-default chain (§8.1) resolves once at
capture/visibility-record creation, and any resolution more restrictive than public is
persisted then, binding the tier immutably so later default drift cannot expose an
already-captured-but-not-yet-served state (Invariant A, §5.4).

### 5.2 Map the audiences against the *real* `visible()` table — do not reuse `Internal` for private

The reader-side filter `visible()` (`visibility.rs:148`) switches on two axes: the
*content's* tier (`AnnotationVisibility`) and the *reader's* tier (`AudienceTier`).
Grepping the actual arms (not the module doc-comment) surfaces two facts that
disqualify the issue's first-instinct "private = `Internal`" mapping:

1. **The `Internal` *audience* is all-seeing.** Arm `(_, AudienceTier::Internal)
   => true` (`visibility.rs:153`) means a reader holding `AudienceTier::Internal`
   sees **every** content tier. No content value is hidden from it.
2. **`Internal` *content* is one of the *least*-restrictive values, not the most.**
   `AnnotationVisibility::Internal` content is hidden only from the `Public` and
   `Restricted` audiences (arm `visibility.rs:155-156`); it is **visible to every
   `Team(_)` audience** (arm `visibility.rs:160`) and to the all-seeing `Internal`
   audience (`:153`). Mapping the issue's `private` tier to `Internal` would make an
   embargoed security-fix commit readable by **any** team-scoped *or* internal
   caller — that is a soundness hole, not an embargo.

The strictest value the *current* table can express is `Restricted { scope_label }`:
hidden from `Public`, from **all** `Team` audiences, and from every non-matching
`Restricted` label (arms `visibility.rs:166-169`); visible only to a reader holding
the exact matching `Restricted(label)` — **but still to the all-seeing `Internal`
audience** (`:153`). `TeamScoped { team_id }` is equally strict, keyed on a team
rather than a label. So the corrected mapping is:

| Issue audience    | Content tier | Hidden from (per `visible()`) | Visible to |
|-------------------|--------------|-------------------------------|------------|
| public            | `Public` | (nobody) | every audience (`:151`) |
| reviewer-scoped   | `Restricted { scope_label }` (or `TeamScoped { team_id }`) | `Public`; all non-matching `Team`/`Restricted` (`:166-169`) | the matching `Restricted(label)`/`Team(name)`, **and** the all-seeing `Internal` (`:153`) |
| private (embargo) | **new strictest `Private { scope_label }` tier** (recommended; see below) | **every** audience, *including* `Internal`, except the one authorized scope | only the authorized `Restricted(label)` |

**Why `private` needs a new tier, not a reused one.** Even `Restricted` content is
visible to the all-seeing `Internal` audience (`:153`). An embargo whose soundness
hinges on "the grant→tier mapping must never hand an untrusted puller
`AudienceTier::Internal`" is fragile — that mapping lives in the **closed weft
repo** (§2.6, §9 O1/O2) and cannot be audited from this workspace. The recommended
design therefore adds one strictest content tier and **one explicit `visible()`
arm**:

```rust
// planned — extend visible() in crates/repo/src/visibility.rs.
// These arms MUST sit ABOVE the existing `(_, AudienceTier::Internal) => true`
// arm (visibility.rs:153): match arms evaluate top-to-bottom, so a Private arm
// placed below it would never be reached for an Internal audience, and the
// embargo would silently leak to internal callers.
(VisibilityTier::Private { scope_label }, AudienceTier::Restricted(viewer))
    if scope_label == viewer => true,
(VisibilityTier::Private { .. }, _) => false,
```

This makes the embargo hold **by construction**: `Private` content is withheld from
`Public`, every `Team`, every non-matching `Restricted`, *and* the otherwise
all-seeing `Internal` audience — visible only to the one authorized scope. (Adding
these arms narrows the all-seeing property: `AudienceTier::Internal` becomes
"sees everything *except* `Private`".) The security-fix worked example (§7.1) is
sound under these semantics: a public/anon puller (`AudienceTier::Public`) and any
internal or team caller are all denied `N`'s content; only a holder of the
authorized embargo scope sees it. The distinction from `reviewer-scoped`
(`Restricted`) is exactly the `:153` escape hatch — `Restricted` content is
deliberately visible to the internal trusted set (so internal CI/tooling can read
an in-flight PR), whereas `Private` is withheld even from it.

**Enum shape.** Promote `AnnotationVisibility` to a shared `VisibilityTier`
(annotations, discussions, *and* states) and add the `Private { scope_label }`
variant + the `visible()` arm above. This is one decision with O4 (enum
unification, §9). If the maintainer prefers isolation, a `StateAudience` enum with
the same variants is the fallback, at the cost of duplicating the (now-extended)
`visible()` table.

**Alternative (no new `visible()` arm).** If the maintainer wants zero changes to
the existing filter, map `private → Restricted { scope_label }` (the strictest
*existing* value). This is sound **only** under an explicit, load-bearing
precondition: the grant→`AudienceTier` mapping (weft, §2.6) must map every
unauthorized puller to `AudienceTier::Public` — never `Internal` — and must never
mint a `Restricted(label)` matching the embargo scope for an unauthorized caller.
Because that precondition lives in closed code and defeats the embargo silently if
violated (`Restricted` content is fully visible to `AudienceTier::Internal` via
`:153`), it is the *fallback*, not the recommendation. The primary design removes
the dependency on it.

The **reader** side needs no new vocabulary either way: an authorized embargo holder
operates as `--audience restricted:<embargo-label>`, reusing the already-shipped
`AudienceTier` string grammar (`internal | public | team:NAME | restricted:LABEL`,
`visibility.rs:60-87`).

### 5.3 Enforcement — one downward-closed reachability gate (hard); an operator-local stub is only a courtesy

There is exactly **one** enforcement mechanism for the public mirror, and it is
the same computation on the wire serve and on the Git-bridge export: the
**tier-aware, downward-closed reachability gate**. This is the r3 tier-aware
reachability gate and the r5 descendant-withholding rule **unified into a single
pass — not two mechanisms**.

**The gate.** For audience `A`, the served set of states is the maximal
**ancestry-closed** set all of whose members are visible to `A` — i.e. a state
`S` is served iff `S` **and every ancestor of `S`** are visible to `A`
(visibility resolved from the sidecar + the caller's `AudienceTier`, §2.6).
Serve exactly the **forward closure** (trees, blobs, sidecars) of that set;
everything else is absent.

**Frontier-before-emit — one categorical invariant over the whole class of
ref-publishing surfaces, with no carve-out.** The gate is *not* "walk from the
tip and stop when you hit an under-tier state." It is a **pre-pass**: *before*
anything is emitted or any ref is moved, resolve the per-audience **visibility
frontier** — in a linear thread the *last visible ancestor* (the deepest state
all of whose ancestors-and-self are visible to `A`); in a merge DAG the
**antichain of maximal served states** (the cut across the DAG, defined under
"Merge DAGs" below) — and root every surface's output at that frontier.

State the rule **categorically**, as a property of the *surface class* rather
than a checklist of names: **every surface that publishes or syncs a ref, or
emits served state — branch refs, `ListRefs`, the Git-bridge branch sync, the
Git-bridge marker→tag sync, the Git-bridge state-note write
(`refs/notes/heddle`), the `HEAD` symref, the bulk export/push, *and any surface
added later* — MUST resolve the visibility frontier before it publishes, and
MUST NOT publish or sync from the raw thread tip / raw mapped state.**
**Categorical backstop:** any surface that publishes a ref or emits state
computes the frontier first — *no exceptions*. A surface that skips the pre-pass
is a bug *in that surface*, not a gap in a list here; the invariant — not the
enumeration — is the gate, so a not-yet-listed surface is never a silent
soundness hole.

The ref-publishing / state-emitting surfaces are enumerated below **exhaustively
as of this audit** — grepped across the bridge
(`crates/cli/src/bridge/{git_export,git_sync,git_notes,git_core}.rs`) and the
wire (`crates/proto/src/{message_refs,message_pushpull,object_graph}.rs`); the
authoritative weft serve path is gated by this same categorical rule, not by
this list. **Six** surfaces, each the same frontier projected:

1. **Wire closure planner** — `enumerate_state_closure_with_options`
   (`crates/proto/src/object_graph.rs:59`) roots its closure emission at the
   frontier, never at the requested tip.
2. **`ListRefs`** — (`RefEntry` / `RefsList`,
   `crates/proto/src/message_refs.rs:82-91`) lags the advertised ref's
   `change_id` / `head_state` to the frontier, never dropping an already-public
   ref.
3. **Git-bridge branch ref-sync** — the `export_scoped` thread loop
   (`crates/cli/src/bridge/git_export.rs:277-288`) advances `refs/heads/main` to
   the **frontier's** mapped commit, *not* to the real `get_thread` tip
   (`git_export.rs:278`).
4. **Git-bridge marker→tag sync** — the *same* `export_scoped` run, on a
   whole-repo export (`thread.is_none()`), loops over markers and publishes each
   as a Git tag: `list_markers` → `get_marker` → `mapping.get_git` →
   `sync_marker_to_tag` (`crates/cli/src/bridge/git_export.rs:290-296`;
   `sync_marker_to_tag` at `crates/cli/src/bridge/git_sync.rs:157`). This loop
   reads the marker's mapped state OID **directly** (`:293-294`), decoupled from
   the branch frontier — so a marker pointing at an embargoed (but
   already-mapped) state would publish the hidden commit as `refs/tags/<marker>`
   even while `refs/heads/main` correctly lags. A tag names a **specific** state,
   not a moving tip, so it cannot lag to an ancestor; the frontier rule for a tag
   is therefore **withhold the tag entirely until its marked state is itself
   served** (the marked state and all its ancestors visible to `A`), then publish
   it forward on disclosure. (`sync_marker_to_tag` is conflict-on-mismatch, not
   fast-forward — `git_sync.rs:163-170` — so a tag once published at the hidden
   OID cannot even be silently corrected later; gating before the publish is the
   only sound path.)
5. **Git-bridge state notes (`refs/notes/heddle`)** — the export loop attaches a
   per-commit `HeddleNote` carrying the state's `change_id`, `agent`
   (provider/model), `confidence`, `status`, `attribution` (principal
   name/email), and risk `signal_counts` (`HeddleNote` /
   `HeddleNote::from_state`, `crates/cli/src/bridge/git_notes.rs:33-56,100-118`;
   ref constant `NOTES_REF = "refs/notes/heddle"`, `:29`). Notes are written at
   **three** frontier-decoupled sites: the mint loop (`git_export.rs:242-245`),
   the post-mint backfill that iterates **every** entry of `bridge.mapping`
   (`git_export.rs:252-261`), and the mirror-build path (`git_core.rs:1105-1109`)
   — none consults audience or the frontier. The notes ref then mirrors
   **forced** (`+refs/notes/*:refs/notes/*`, `git_core.rs:300`), so a plain
   `git clone` / mirror of the public remote picks up the whole notes tree
   out-of-band of the branch tip. A note is precisely the **Git-mirror analogue
   of the wire `ObjectType::State` header leak** (cid 3324616942): it republishes
   an embargoed commit's `change_id` + attribution + agent + signal metadata even
   when the commit's tree/blobs are withheld. The frontier rule for notes is
   therefore the **same gate as the State header** — a note for state `S` is
   written and published **only when `S` is itself served** (`S` and all its
   ancestors visible to `A`); the backfill loops must filter by the served set,
   never iterate the raw mapping. Like a tag, a note names a *specific* commit and
   cannot lag to an ancestor — withhold it entirely until its state discloses,
   then publish forward.
   **But for this ref, filtering the current write/tree is *not* sufficient,
   because the notes ref is *history-bearing*.** `write_note` reads the prior
   notes head and **parents each new notes commit on it** (`read_notes_head` →
   `git_notes.rs:180`; `new_commit_as(sig, sig, msg, tree, parents=[prev_head])`
   → `git_notes.rs:195-204`), so `refs/notes/heddle` is its **own accreting commit
   chain**: every note ever written for a state that is **still embargoed** (never
   validly served to this audience — e.g. a legacy/pre-gate note written before the
   audience gate existed) stays reachable through the ref's **parent chain** even if
   the *current* (HEAD) notes tree omits it. The forced `+refs/notes/*` mirror
   (`git_core.rs:300`) transfers
   the **whole chain**, so publishing the ref still ships the embargoed note's
   blob/tree out-of-band and leaks its `change_id`/attribution/agent/signal
   metadata. Filtering only the tip tree is the notes analogue of the unsound
   "swap the published stub" fix (§5.0.1) — it changes what the *current* object
   says while leaving the hidden object reachable. The sound rule is therefore to
   **REBUILD the published notes ref**: before public export/push, reconstruct the
   notes commit chain (squash/rewrite) so that **no embargoed-state note object is
   reachable through the published ref's history** — a notes commit (or chain)
   whose every reachable tree maps only served states. The rebuild applies **only to
   never-public (still-embargoed) state notes**; it is **not** a mechanism to re-hide
   a note for an already-served state. Re-embargoing an already-served state is
   rejected outright (§5.4 — you cannot un-send bytes: a public puller may already
   hold the note blob/tree), so a note once validly served stays published and the
   rebuild only ever excludes note objects that should never have been reachable to
   this audience. This is the §5.0/r4–r5 content-addressing principle (*you cannot
   hide an object that is still reachable
   from a published ref*) applied to an **auxiliary, history-bearing ref**, and it
   generalizes immediately below.
6. **Symref / bulk-publication surfaces — `HEAD` and the whole-mirror
   export/push.** `HEAD` is written as a symref to a branch
   (`ref: refs/heads/<branch>`, `git_core.rs:2398-2407`); it is sound **iff** its
   target branch is itself frontier-lagged (surface 3) — a symref resolves
   *through* the branch, so a lagged `main` keeps `HEAD` lagged too — but `HEAD`
   must never be pointed at a wholly-embargoed branch that surface 3 left absent
   (resolve `HEAD` to a served branch, else omit it). The bulk paths —
   `export_to_path` (`git_core.rs:751`) and `push` / `push_with_scope`
   (`git_core.rs:678-689`) — publish **all** of `refs/heads/*` + `refs/notes/*`
   in one shot and carry no per-surface logic of their own; they inherit
   soundness entirely from surfaces 3–5 having already lagged/withheld each ref
   *before* the bulk copy runs. Listed so the implementer treats "copy the whole
   mirror" as gated-by-construction, never as a separate bypass.

**History-reachability — classify every surface as single-target vs
history-bearing.** The notes leak above is not a notes-specific quirk; it is the
§5.0/r4–r5 principle (*an object stays exposed as long as it is reachable from a
published ref, regardless of what the ref's tip currently names*) applied to a
ref that maintains **its own commit chain**. So the surface class splits in two,
by *what is reachable from the published ref*:

- **Single-target / frontier-governed surfaces** — the published ref names one
  commit and everything reachable from it is the **project state-DAG**, which the
  downward-closed frontier invariant (§5.0/§5.3) already holds served. Lagging the
  tip (branch ref, surface 3; `HEAD` symref, surface 6) or withholding it
  (marker→tag, surface 4) is sufficient, because the history reachable from the
  published target is served *by construction of the frontier*. The set-emitting /
  per-call surfaces (wire closure planner, surface 1; `ListRefs`, surface 2)
  likewise recompute from the frontier per request and accrete no history.
  **Filter / lag / withhold suffices.**
- **History-bearing auxiliary surfaces** — the published ref maintains a commit
  chain **distinct from** the project state-DAG, where each update is a new commit
  *on top of* the prior head, so its reachable history accretes every past write
  regardless of the current tip's tree. The frontier does **not** govern this
  chain (there is no all-public ancestor notes commit to lag to — served and
  embargoed notes interleave in write order). **Filtering current content is never
  sufficient; the ref MUST be rebuilt on withhold** so no embargoed object is
  reachable through its published history. Today the lone such surface is **state
  notes** (surface 5, `refs/notes/heddle`), and because the mirror refspec is
  `+refs/notes/*` (`git_core.rs:300`) **any** future `refs/notes/<x>` falls in
  this bucket automatically.

**General rule (stated so a reviewer cannot find a second leaking history-bearing
surface):** *any published ref that accretes its own commit history — notes refs
today; any future history-bearing auxiliary ref (another `refs/notes/*`, a
replace-ref, a synthetic metadata ref) — MUST be rebuilt so embargoed objects are
unreachable through that ref's published history; filtering the current content is
never sufficient for a history-bearing ref.* The `resolve_frontier` chokepoint
below therefore returns, for a history-bearing ref, **not a lagged tip but a
rebuilt ref** (a reconstructed chain excluding every embargoed-state note object);
single-target refs get a lagged/withheld tip as before.

**The rebuild stays forward-only (r4/r5-consistent).** Rebuilding the notes ref
for publication is forward-only *from the public consumer's view*: a note for an
already-served state, once published, keeps stable content (a note is
identity-stable like the served commit it annotates), and because tiers are
one-way (§5.4 — a served state never re-embargoes) the set of published notes only
**grows**. The rebuild never rewrites or drops a note that was already public for a
served state; it only excludes embargoed-state note objects from reachability and
appends the newly-served state's note forward on disclosure. (The notes ref's
*commit OIDs* may churn under rebuild — acceptable because it is an out-of-band
auxiliary ref carried by the forced `+refs/notes/*` mirror, not project history
under the FF guard; the forward-only guarantee is on the **observable note
content**, never on the notes-commit OIDs. This is the one sanctioned ref rewrite,
and only because it removes never-should-have-been-public objects — it does **not**
license the §5.0.1-forbidden rewrite of already-published *public* history.)

**Close the class structurally, not by extending this list.** The recurring leak
across r6 (closure planner), r7 (`ListRefs` + the git_export branch sync), r8
(marker→tag), and — anticipated this round — r9+ (state notes, the symref, the
bulk push) is the *same* bug class: a surface syncing from the raw tip/state /
publishing per-state metadata, re-appearing at a new surface. The durable fix is
a single shared chokepoint: every ref-publishing **and** state-emitting surface
routes its target through one `resolve_frontier(audience, raw_target) ->
served_target` helper (returning a lagged ref for a moving tip, or *absent* for a
tag / note / ref whose marked state is not served), so a new surface that wires a
raw OID into `sync_track_to_branch` / `sync_marker_to_tag` / `write_note` / the
closure root **without** passing through the helper is the bug, and the helper —
not vigilance over a list — is what makes the class impossible. (Follow-up impl
issue, §10 #4/#5: extract the frontier resolver + route **all six** surfaces
through it; a conformance test asserts no ref-publishing or note-writing call
site takes a raw `get_thread` / `get_marker` / `mapping`-iterated OID.)

A topological walk-halt that skips *minting* under-tier states is at most an
optimization layered on top of this rule — it is **never** the gate, because the
ref-sync and note-write surfaces (3, 4, 5) read the raw tip / raw marked state /
raw mapping independently of which states the walk happened to mint (the note
backfill at `git_export.rs:252-261` iterates the *whole* mapping, so it writes a
note for an already-mapped embargoed state the mint-halt never touched). Each
host below realizes the rule for its surface(s):

- **Wire serve (weft, authoritative) — visibility is a PRE-PASS, computed before
  any object is emitted, and `ListRefs` lags rather than drops.** The closure
  planner cannot be gated by "walk from the tip and halt when the walk *later*
  reaches an under-tier ancestor": that gate fires too late.
  `enumerate_state_closure_with_options` (`crates/proto/src/object_graph.rs:59`)
  seeds its queue with the *requested tip* (`:70`) and, on the very first pop,
  pushes that state's `ObjectType::State` object into `out` (`:85-90`) and then
  its whole tree closure (`:98-104`) **before** it enqueues the parents
  (`:92-96`) — so an ancestor's visibility is not consulted until a later
  iteration, by which point the descendant tip's `State` (carrying
  `intent`/`attribution`/`signature`) and its contents have already been emitted.
  (The plan-only variant `enumerate_state_closure_plan_with_options` has the
  identical order: state pushed at `:160-163`, parents enqueued at `:165-169`,
  tree at `:171-177`.) The gate must therefore run as a **pre-pass that resolves
  the served frontier before any emission**: walk the requested tip's ancestry,
  find the **last visible ancestor** — the deepest state all of whose
  ancestors-and-self are visible to `A` — and root the closure emission at *that*
  frontier, not at the requested tip. The planner then only ever emits states
  known-visible up front; an under-tier state's `ObjectType::State` object (hence
  its `intent`/`attribution`/`signature` header) never leaves the host, closing
  cid 3324616942, and because emission is rooted at the visible frontier the
  under-tier state's **descendants are never enumerated either** — downward-closure
  is enforced at planning time, structurally, not discovered mid-walk.

  **`ListRefs` lags the ref at that frontier; it never drops an already-public
  ref.** A `RefEntry` (`crates/proto/src/message_refs.rs:89`) carries the ref's
  tip as `change_id` (`:91`) and `RefsList.refs` (`:82`, `:85`) is the served
  advertisement. When the real thread tip is a public commit descended from an
  embargoed ancestor, dropping the whole `RefEntry` would **temporarily remove an
  already-public branch** — a non-forward-only, unstable change, the opposite of
  the r4/r5 forward-only guarantee (§5.0, §7.1). Instead `ListRefs` rewrites the
  ref's `change_id` to the served frontier the pre-pass computed (lag `main` at
  the last all-public ancestor `N-1`), and `head_state` (`:84`) lags to that same
  frontier. The ref stays stable and forward-only — it advances only as ancestors
  disclose (§5.4). A ref is **absent** only when *no* ancestor is visible (a
  wholly-embargoed thread with no public ancestor — e.g. a pre-review
  `private:<author>` feature thread, §7.2), because then there is no
  last-visible-ancestor to lag to.
- **Git-bridge export (heddle OSS) — the ref-sync is a frontier pre-pass too, not
  a walk-halt.** `export_state` (`git_export.rs:28`) already loads the `State`
  (hence its `ChangeId`) but currently passes only `&state.tree` to `export_tree`
  (`git_export.rs:41`) and takes no audience (its `--audience` is planned — inline
  note at `git_export.rs:44-47`); add an `AudienceTier` parameter so minting is
  audience-aware. **But minting is not where the mirror's tip is decided — the
  ref-sync is, and it must run the same pre-pass.** After the mint loop, the
  `export_scoped` ref-sync loop reads the **real thread tip** via `get_thread`
  (`git_export.rs:278`), maps it through `mapping.get_git` (`:279`), and advances
  the branch to that commit with `sync_track_to_branch` (`:281`) — a step
  **decoupled from the mint walk**: it points `refs/heads/main` at wherever the
  *raw* tip maps, regardless of which states the walk minted. (The mapping is also
  pre-seeded from existing mirror objects — `build_existing_mapping` at `:198`,
  `retain_git_objects` at `:203` — so a tip can map to a commit the current run
  never touched.) A topological mint-halt is therefore **necessary but not
  sufficient**: even if `sort_states_topologically(&states)` (`git_export.rs:201`)
  is walked ancestor-first (`:213`) so under-tier descendants are never *minted*,
  the ref-sync at `:278-281` would still try to advance `main` from the raw tip —
  leaving it either stuck at a stale prior tip (failing to lag forward to the new
  frontier) or, if the tip already maps to a commit, advancing past the embargo
  boundary. The bridge must therefore compute the **same visibility frontier as
  surfaces 1–2 before the ref-sync at `:277-288`** and pass the **frontier's**
  mapped commit (the last all-public ancestor) to `sync_track_to_branch`, exactly
  as `ListRefs` lags `change_id` to that frontier — never the `get_thread` tip's.
  With the ref pinned at the frontier and `refs/heads/main` fast-forward-only
  (`ensure_commit_update_fast_forward`, §5.0), the embargoed commit and its
  descendants are absent from the mirror, and the ref advances only as ancestors
  disclose. This is the Git-mirror projection of the very same gate — the public
  mirror only ever holds the visible, ancestry-closed closure. The **marker→tag
  loop in the same run** (`git_export.rs:290-296`, surface 4 above) carries the
  identical obligation: a marker whose marked state is not served must yield
  **no** `refs/tags/<marker>`, not a tag at the raw mapped OID. The **state-note
  writes in the same run** (surface 5: `git_export.rs:242-245` mint loop,
  `:252-261` whole-mapping backfill, and `git_core.rs:1105-1109`) carry it too:
  no `HeddleNote` is written or pushed for a state that is not served — otherwise
  the forced `+refs/notes/*` mirror (`git_core.rs:300`) republishes the embargoed
  commit's `change_id` / attribution / agent out-of-band even with
  `refs/heads/main` correctly lagged.

**Merge DAGs — the frontier is a cut, not a point.** A `State` carries
`parents: Vec<ChangeId>` (`crates/objects/src/object/state_core.rs:207`; merges
are minted via `new_merge`, `:276`), so a thread is a DAG, not a line, and a
single hidden merge can leave **several** maximal visible ancestors on different
parent paths. The served-set definition already handles this without amendment: a
state `S` is served iff `S` **and every ancestor of `S` on every parent path** are
visible to `A` (the gate above is defined over the full ancestor set — `parents`
is a vector, not a single edge). So the *served set* is always unambiguous: the
maximal ancestry-closed visible set. What generalizes from a point to a set is the
**frontier** itself = the **antichain of maximal served states** (the served
states having no served descendant — the cut across the DAG).

**The full merge tail — octopus, criss-cross, and how the antichain has no
special cases.** Two DAG shapes that look like edge cases are handled by the same
gate without amendment, *because the gate never computes a merge base — it is
defined purely by per-state transitive ancestor visibility*:

- **Octopus merges (>2 parents).** `parents: Vec<ChangeId>` is unbounded, so a
  merge may have `k > 2` parents and a hidden octopus merge can leave up to `k`
  incomparable maximal visible ancestors. The antichain is simply larger; the
  multi-root advertisement (below) carries `k` roots exactly as it carries two.
- **Multiple / criss-cross merge bases.** When two lines were cross-merged there
  are several merge bases. Merge-base *count* is irrelevant here: the served-set
  gate selects no base and walks no base — it asks only "is every transitive
  ancestor of `S`, on every path, visible to `A`?" So criss-cross history is not
  a special case; a state is served iff its whole transitive ancestor set is
  visible, full stop, and the frontier is still the maximal served states.

**How the frontier is computed.** A per-audience pre-pass over the requested
tip's reverse-reachable ancestor DAG: resolve each state's tier (sidecar +
`A`, §2.6) to visible/hidden, then a single topological pass marks a state
*served* iff it is visible **and every parent is served** (served-ness is the
least fixed point of "visible ∧ all-parents-served"). The frontier is the
antichain of served states with no served child. Cost is `O(states + edges)`
over the ancestry — no merge-base computation, no per-request filtering that could
misfire.

**How the frontier is transmitted without leaking.** The puller receives exactly
(a) the forward closure of the served set and (b) the antichain advertised as
refs (below) — *nothing else*. It is never told how many states were withheld,
where the gaps are, or that a merge ancestor exists beyond the frontier: a
withheld state contributes no object, no count, no placeholder, and its
visibility record is itself gated (§8.4). Because the served set is
ancestry-closed, every served state's parents are also served — the puller never
sees a parent edge dangling into the embargo. Absence is indistinguishable from
non-existence: the puller cannot tell "this thread ends at the frontier" from
"there are hidden merge ancestors past it." That indistinguishability *is* the
transmission guarantee.

The two surface kinds consume the antichain differently:

- **Set-emitting surfaces** (the wire closure planner, surface 1) root emission at
  **all** maximal served states and emit the forward closure of the *whole*
  ancestry-closed visible set — every visible side of a pre-merge fork. The
  antichain loses nothing: no visible commit is ever silently omitted.
- **Single-tip ref surfaces** (the Git branch ref, surface 3; the marker tag,
  surface 4; the state note, surface 5; each individual `RefEntry`'s
  single-`change_id`, surface 2) can name only **one** commit apiece. The *moving*
  ref `<thread>` advances **forward along its own line**: to the **unique maximal
  served descendant of its currently advertised tip** — as far up its own
  descendant chain as that chain stays unique — and it holds its prior tip *only*
  when its own line has no unique forward successor (its prior tip is already a
  frontier member, or its own served descendants themselves fork into ≥2
  incomparable served states). It is **never** moved onto a *sibling* antichain
  member (a maximal served state that is *not* a descendant of its prior tip): that
  would be a lateral, non-fast-forward move (§5.0). Crucially, the advance is
  decided over the prior tip's *own* descendants alone — a sibling line being
  maximal elsewhere never freezes this ref short of its own-line maximum (e.g. it
  advances `A → A2` even while a sibling `B` is also maximal). The complete
  placement rule is stated under "General rule" below. On its own this still leaves
  the *other* maximal served states (the sibling lines) with no ref naming them;
  that gap is closed by the multi-root advertisement path next.

**The protocol path for all merge-frontier roots (closes cid 3325554155).** "The
full visible set stays fetchable" is only true if every maximal served state is
both *discoverable* and *requestable*. The single moving ref cannot do that for an
antichain of size ≥2, so each surface gets an explicit multi-root path, and none
requires widening the single-`ChangeId` request shape:

1. **Wire advertisement — multi-root, no new wire field.** `RefsList.refs` is
   already a `Vec<RefEntry>` (`crates/proto/src/message_refs.rs:85`), so `ListRefs`
   advertises **one `RefEntry` per maximal served state** in the antichain. The
   moving ref `<thread>` names the **maximal served descendant on its own line** —
   the unique maximal served descendant of its prior advertised tip — and **every
   *other*** maximal served state (every member on a *sibling* line, i.e. not a
   descendant of `<thread>`'s prior tip) gets its own `RefEntry` under a
   **deterministic derived name** `<thread>@<full-changeid>`, the name a pure
   function of the served `ChangeId` (`RefEntry.change_id`, `:91`) so it is stable,
   identity-bound, and never reused for a different state. The name carries the
   **full** `ChangeId` — `ChangeId::to_string_full()` (`crates/objects/src/object/hash.rs:129`,
   the `hd-`+base32 form that round-trips via `parse()`, `:143`) — **never** the
   truncatable `short()`/`Display` form (`:137`, `:167-170`): two distinct sibling
   `ChangeId`s must map to two distinct ref names or one frontier root would
   overwrite the other (Invariant C, §5.4). So `<thread>` **advances
   forward** as its own line extends (`A → A2` once `A`'s descendants become
   served) and is **never frozen at a stale ancestor** just because a sibling line
   is also maximal; it is likewise **never assigned by topological or lexicographic
   order** among the members, since that could move it *sideways* to an incomparable
   sibling — a non-fast-forward move a client cannot distinguish from a rewritten
   ref, the exact opposite of the §5.0/§7.1 forward-only guarantee. (If `<thread>`
   has *no* prior advertised member at this audience — the thread's very first
   advertisement already forks into an antichain, so there is no prior tip to define
   "own line" — the anchor is fixed by the **initial-anchor rule** (rule 0 of the
   definitive statement below): the member whose **`ChangeId` is least by raw byte
   order** (`ChangeId` is `[u8; 16]`, `crates/objects/src/object/hash.rs:99`), a
   host-independent per-state identity key applied a single time and then persisted as
   a fact, so every export/serve host adopts the **same** member; chosen once, never
   re-chosen, and thereafter advanced only forward along that chosen line.) The antichain is
   thereby **discoverable**: the client reads every served root out of the one
   `Vec`, with `<thread>` advancing forward on its own line.
2. **Wire request — single-root pull, issued per root.** Each `Pull` carries one
   `PullRequest.target_state` (`crates/proto/src/message_pushpull.rs:91`) and each
   `PullReady.remote_state` echoes one served root (`:118`), so the client issues
   **one pull per advertised root**, passing the states already received in
   `exclude_states` (`:95`) so blobs/states shared across the sides are not re-sent
   (content-addressing dedups the overlap). The union of the per-root closures is
   the whole served set — so the antichain is **requestable** with the *existing*
   single-root request shape; the multiplicity lives in the advertisement and in
   issuing N pulls, not in a widened `target_state`.
3. **Git mirror — deterministic synthetic refs.** `refs/heads/<thread>` (one
   FF-only ref) advances along its own line to the **maximal served descendant of
   its prior tip**, else retains its prior tip — **never sideways to a sibling
   antichain member, never backward**. This is not merely a stated rule: the
   mirror enforces it structurally — `sync_track_to_branch`
   (`crates/cli/src/bridge/git_sync.rs:124`) routes every move through
   `ensure_commit_update_fast_forward` (`git_core.rs:2446`), which admits the new
   tip *only* when `commit_is_descendant_of(new, old)` (`:2455`/`:2468`) and else
   raises `NonFastForwardRef` (`:2457`). An own-line advance `A → A2` is a
   descendant and is accepted; a lateral jump to a sibling `B` or a regress to an
   ancestor is rejected. Each *other* incomparable maximal served
   state is published under a deterministic synthetic branch
   `refs/heads/<thread>@<full-changeid>` so a plain `git clone` can still fetch
   every visible side. Adding a synthetic ref is
   forward-only (it never rewinds `<thread>`); the names are pure functions of the
   served `ChangeId`, so re-runs are deterministic and no published ref is ever
   rewritten (r4 identity-stability). A synthetic ref retires **only** once a
   served descendant has reunified the fork and `<thread>` has advanced past its
   state — at which point the state is reachable from `<thread>`, so removing the
   redundant pointer loses no reachability (it is not "dropping an already-public
   ref": access to the commit survives via `<thread>`).

**General rule — antichain ref placement, the definitive statement (closes cid
3326047821; refined by cid 3326161852; initial anchor added per cid 3326236661).**
A served *moving* ref (`<thread>` on the wire, `refs/heads/<thread>` on the mirror,
and every single-tip ref) obeys five rules, exhaustively. Rule 0 establishes the
ref's "own line" at the initial condition; rules 1–4 govern every move thereafter:

0. **Establish the anchor (the initial condition rules 1–4 presume).** Rules 1–4 all
   move a ref *relative to its prior advertised tip* — its "own line." When the thread
   has a prior advertised member at audience `A`, that member **is** the own line and
   rules 1–4 govern directly. When it has **none** — the thread's very first
   advertisement at `A` already forks into an antichain of ≥2 maximal served states,
   so there is no prior tip to define "own line" — the anchor is fixed by a
   **content-intrinsic, host-independent, one-time tiebreak**: `<thread>` adopts the
   antichain member whose **`ChangeId` is least by raw byte order** (`ChangeId` is
   `[u8; 16]`, `crates/objects/src/object/hash.rs:99`, deriving `Ord`/`PartialOrd` at
   `:98` — lexicographic over the 16 identity bytes; the anchor is `min` over the
   antichain members' `ChangeId`s, the same value `RefEntry.change_id` carries on the
   wire, `crates/proto/src/message_refs.rs:91`). This tiebreak is:
   - **host-independent.** A `ChangeId` is the state's *persisted identity*, assigned
     once at change creation (`generate()`, `hash.rs:103`) and replicated **verbatim**
     to every host (it travels in `RefEntry.change_id` and in every synced record), so
     the same antichain yields the same `min`-member on **every** export/serve host,
     wire or mirror. The git commit `ObjectId` is deliberately **not** the key: it is
     minted only at git-export time (`git_export.rs:84-93`), so a wire-only serve host
     that never mints commits could not compute it, and it would diverge from the wire
     anchor; `ChangeId` exists on every host before any mint. The mirror's `<thread>`
     follows the `ChangeId`-selected anchor through the `ChangeId`→OID map, so wire and
     mirror name the **same** state.
   - **not the ordering rule 2 forbids.** Rule 2 bars *topological/lexicographic
     placement* because re-applying a positional or name order on a **mutating**
     antichain moves the ref *sideways* — a non-fast-forward jump a client cannot
     distinguish from a rewrite. This key is different in kind: a per-state
     **identity** order (independent of serve order, tier, host, wall-clock, and DAG
     position) applied **exactly once** to bootstrap when no own line yet exists, then
     **frozen** — never re-applied as members enter or leave the antichain. The
     prohibition is on ordering-based *re-placement*; the anchor is a one-time
     identity tiebreak, not a re-placement.
   - **computed once by the first advertiser, then read — never re-derived from a
     local antichain.** The anchor is an instance of the **persisted-fact principle
     (§5.4):** it is computed **exactly once, by the authoritative first advertiser** —
     the single host that legitimately has no own-line record because it is *minting*
     it — as `min`-`ChangeId` over *its* antichain at first advertisement, written as a
     **persisted, signed, monotonic fact** (`<thread>`'s own-line record at `A`) and
     replicated host-to-host over the same authoritative record-sync substrate as
     visibility/promotion records (`crates/client/src/grpc_hosted/sync.rs:268-302`,
     §5.4). Every **other** host **reads** that fact; it **never** recomputes
     `min`-`ChangeId` from its own current antichain. The hazard this forecloses is the
     §5.4 *lagging-fact-set* one, not merely a mutable-input one: the byte-order-least
     tiebreak is deterministic only *over a fixed set*, but a host's antichain is a
     function of the facts **it** has synced, and under replication lag two hosts hold
     **different** sets — so a replica that had already synced/promoted a sibling the
     first advertiser had not (one with a lower `ChangeId`) would recompute a
     **different** `min`-member and mint a conflicting anchor, breaking "chosen once,
     frozen." Therefore a host that does **not** yet hold the persisted anchor record
     **defers advertising `<thread>` until it has synced that record** (the
     propagate-before-use half of §5.4); it does **not** bootstrap a second anchor from
     its local set. Only the first advertiser ever computes the anchor; every other
     host reads the propagated fact, and replication lag is governed by §5.4 (await the
     fact / fail toward last-known-public, never re-hide), never by re-anchoring. A
     genuine *simultaneous* first advertisement on two hosts with divergent sets — where
     there is **no** single first advertiser, so each host legitimately mints an anchor
     fact and **neither supersedes the other** (anchors carry no more-open ordering, so
     the monotonic-superseding-record rule cannot pick a winner) — is resolved by
     **Invariant B (§5.4)**: the two conflicting anchor facts are merged by the **same**
     content-intrinsic key r15 fixed for initial selection — the anchor's `ChangeId`,
     least by raw byte order (`hash.rs:98-99`). Every replica that holds both facts
     deterministically adopts the byte-order-least anchor and the other anchor fact is
     **superseded by that merge rule**; all replicas converge with **no lease and no
     single-writer**. The key thus does double duty: `min`-selection within a single
     host's antichain (the initial-anchor rule) *and* conflict-free merge across
     concurrent anchor facts (Invariant B).

   Once the anchor exists it **is** the own line, and rules 1–4 take over verbatim;
   the anchor is never re-selected. **Behavior when the anchored line's visible tip
   changes** is therefore fully determined by rules 1–3: a served descendant appearing
   on the anchored line advances `<thread>` forward to it (rule 1, `A → A2`); the
   anchored line forking into ≥2 incomparable served descendants, or *only* a sibling
   line gaining a served tip, **holds** the ref at its last served member (rules 2–3 —
   a hold, never a lateral hop, never a regress). Because §5.4 forbids re-embargoing an
   already-served state, the anchor never disappears once advertised: a not-yet-served
   descendant is simply not an advance, so the ref holds its last visible member rather
   than regressing or re-anchoring.

1. **Advance along its OWN line.** On each re-advertisement the ref moves forward to
   the **maximal served descendant of its prior advertised tip** — the unique
   maximal state in its prior tip's own served-descendant chain (e.g. `A → A2` as
   `A`'s descendants become served). It does **not** freeze at a stale ancestor.
2. **Never switch lines (no lateral move).** The ref is **never** reassigned to a
   *sibling* antichain member — a maximal served state that is **not** a descendant
   of its prior tip — even when that sibling is also maximal. Sibling lines are
   reached only through their own derived/synthetic names
   (`<thread>@<full-changeid>`, pure functions of the served `ChangeId`), never by
   moving `<thread>` onto them. No ordering-based selection (topological,
   lexicographic) is ever used to *place* the moving ref; ordering is admissible
   only to mint the stable derived names for the non-moving siblings.
3. **Never regress (forward-only).** Once advertised at `A2`, the ref never moves
   back toward `A`. When its own line has no unique forward successor — the prior
   tip is already a frontier member, or its own served descendants fork into ≥2
   incomparable served states — the ref simply **holds its prior tip**; holding is
   not a regress.
4. **Name own-line maximal under a hidden merge.** When a hidden merge leaves
   several incomparable maximal served states, the ref names the **maximal served
   state of its OWN line** (advancing to it per rule 1, not freezing at an
   ancestor); the *other* antichain members are simply **not advertised by THIS
   ref** — they are reached through their own derived/synthetic refs.

**Worked example.** `<thread>` last advertised `A`. Public descendants extend `A`'s
own line so the served chain is `A → A1 → A2` with `A2` maximal on that line;
meanwhile a *sibling* line `B` is also maximal, behind a hidden merge `M` that is
not served (so the antichain is `{A2, B}` and `A` is now **non-maximal**). Then
`<thread>` advertises **`A2`** — its own-line maximal descendant — **never `A`**
(freezing at a non-maximal ancestor violates rule 1) and **never `B`** (jumping to
a sibling line violates rule 2). `B` is published only as `<thread>@<b-changeid>`
(B's **full** `ChangeId`, not a truncatable prefix — Invariant C).
When `M` later becomes served and reunifies the fork, `<thread>` advances FF to `M`
(now a unique dominating descendant on the merged line) and the `B` synthetic ref
retires, since its state is then reachable from `<thread>`.

This is exactly the fast-forward discipline the Git mirror already enforces in
code: `ensure_commit_update_fast_forward` (`crates/cli/src/bridge/git_core.rs:2446`)
admits a ref move **only** when the new tip is a descendant of the old
(`commit_is_descendant_of`, `:2455`/`:2468`), rejecting both lateral and backward
moves with `NonFastForwardRef` (`:2457`); the wire ref `<thread>` mirrors that same
own-line, forward-only discipline (§5.0/§7.1).

**Cross-path disclosure ordering — forward-only regardless of order.** When a
merge `M` has embargoed ancestors on ≥2 different parent paths that disclose at
*different* times, the gate is simply re-evaluated at each disclosure:

- `M` becomes served only when the **last** of its embargoed ancestors across
  **every** parent path discloses (the gate requires *all* ancestors visible). A
  path that discloses earlier advances *its own* frontier forward-only and
  independently — that side's maximal served state moves up — but `M` stays
  withheld until the other side also fully discloses.
- Each disclosure is a fast-forward append on its own path; the order in which the
  two paths disclose changes nothing about identity. `M`'s OID is content-addressed
  and minted exactly once, when it finally becomes served (§7.1 step 5), so no
  published OID changes and nothing is re-ordered or rewritten — identity-stable
  per r4/r5 under every interleaving.
- The antichain shrinks **monotonically**: two incomparable maximal served states
  collapse to the single now-served `M` once both their sides are public, and the
  synthetic ref for the later-disclosing side retires at that point (its state is
  now reachable from the advanced `<thread>`). The advertisement therefore only
  ever loses redundant roots, never a still-needed one.

**Why this is a plain forward-closure, not a set difference (this supersedes the
r3 framing).** r3 correctly rejected reusing `collect_excluded`
(`object_graph.rs:360`): its root-exclusion blanket-drops **every** tree/blob
reachable from an excluded state and its ancestors (`collect_tree_hashes`,
`object_graph.rs:402`), which would over-withhold blobs a visible child shares
with a hidden parent. r3's proposed fix was a *set difference* — serve a visible
child `N+1` while subtracting hidden parent `N`'s exclusive closure. **The
invariant removes the need for that subtraction entirely:** under downward-closure
a child of a hidden parent is **never in the served set**, so the "visible child
shares a blob with a hidden parent" case never arises in what we serve. The
served set is just `closure(ancestry-closed visible states)`; a blob travels iff
it is reachable from at least one served state — plain forward reachability, no
subtraction, and it never over-withholds (a blob shared by served `Q` and hidden
`P` is reachable from `Q`, so it travels on `Q`'s account). The hard part r3
named dissolves; what remains is an ancestry-closed visibility walk rooted at the
visible frontier and bounded by the embargo boundary. (r3's conclusion stands — do **not** reuse
`collect_excluded`'s root-exclusion — but the replacement is simpler than the
set difference r3 proposed.)

**The operator-local stub is a courtesy, never a public-mirror surface.** The
*only* place a stub ever appears is the **operator's own checkout** — the
authorized holder who already possesses the bytes and the embargo records. There,
an under-tier state's working-tree files may render as a short placeholder
(naming the tier + promotion date, like the redaction stub renderer `stub_text`,
`redaction.rs:106`), evaluated at the `goto`/checkout entry that resolves a thread
tip to a `State` → `Tree`, *above* `materialize_tree`
(`repository_materialization.rs:299`), where the `ChangeId` is in scope. It is
rendered from content the holder already has; it is **not** an exposure path, it
never travels to an under-tier consumer, and the public mirror emits **absence**
(the invariant, §5.0) — never a stub.

This predicate is **state-keyed** (per `ChangeId`), so even the courtesy stub
cannot live at the blob-keyed redaction chokepoints: `materialize_blob`
(`repository_materialization.rs:575`) receives only a blob `hash` + a
counter-only `MaterializationContext` (`:57`), and `export_tree`
(`git_export.rs:97`) receives only a `tree_hash` — neither carries the `ChangeId`
or the audience. Redaction works there because it is blob-keyed
(`redaction_stub_for_blob(blob)`, `repository_redaction.rs:509`); state-keyed
visibility is evaluated one level up, at the state walk. The per-blob chokepoints
remain the right home for **blob-keyed** redaction.

### 5.4 Promotion is additive

"Promote visibility" (embargo → public) appends a `StateVisibility` record whose
`supersedes` points at the prior record and whose `tier` is more open — never a
mutation of the state or the prior record. Two triggers:

- **Manual:** `heddle visibility promote <state>` (§8) — the audited "open it up
  now" moment, recorded as an `OpRecord` (§5.5).
- **Scheduled:** an `embargo_until` timestamp is an advisory *trigger*, **not** a
  serve-time predicate. When the **authoritative serve host** (O5) first observes
  wall-clock ≥ `embargo_until`, it **materializes** the promotion *before* the first
  broader-audience serve — appending the same superseding `public` `StateVisibility`
  record + `OpRecord::StateVisibilityPromote` (§5.5) that manual `promote` writes.
  After materialization the state's effective tier is read from that persisted
  record (§5.1), **never recomputed** from `embargo_until` vs wall-clock. So a
  scheduled promotion that has fired-and-been-served is a durable fact, not a clock
  comparison that a skewed/rolled-back clock or a second host could later
  re-evaluate back to `private` (resolves O5).

**Tier-raising is transitive over ancestry (the dual of downward-closure).**
Because the gate (§5.3) serves a state only when **every ancestor** is visible to
the audience, raising one state to a more-open tier `T` has **no observable
effect** unless every ancestor not already visible to `T` is raised to at least
`T` as well — the just-raised state stays withheld behind its still-hidden
ancestors. So any tier-raise — manual `promote`, the scheduled `embargo_until`
lapse, or an open-for-review `set` — must be applied **transitively up the
ancestry**, back to the nearest ancestor already visible to `T` (e.g. the public
merge-base), appending one `StateVisibility` record (§5.5) per raised state. This
is the mirror image of §5.3: downward-closure withholds the **descendants** of a
hidden state; sound promotion must raise the **ancestors** of a revealed one. The
embargoed-fix case (§7.1) satisfies it for free — promoting `N` to `public` while
`N`'s ancestors are already public needs no ancestor raise — but the reviewer-PR
case (§7.2), whose ancestors start `private:<author>`, does **not**, and must
raise the whole thread ancestry to the reviewer tier (see §7.2 step 1). For a
merge-DAG ancestry the raise covers **all** parent paths, matching the
"every-ancestor-on-every-path" served-set rule (§5.3).

**Tier transitions are one-way: a served commit can never be re-embargoed (hard
constraint).** Promotion relaxes the gate for a state; the inverse — *demoting* an
already-served state back to a stricter tier — is **unsound and must be rejected**,
for two grounded reasons that are the exact duals of the §5.0 disclosure
constraints:

- **You cannot un-send bytes.** Once a state's `ObjectType::State` and tree/blobs
  have been served to any under-tier puller (or its real-tree commit published to
  the public Git mirror), the recipient holds the bytes; a later restrictive
  `StateVisibility` record has **no retroactive reach** over distributed content.
  Re-embargo would be soft-hiding at best (§1.1) — security theatre, not an
  embargo.
- **FF-only forbids un-publishing on the mirror.** Removing an already-published
  commit from `refs/heads/main` is a non-fast-forward rewind, which the FF guard
  rejects (`ensure_commit_update_fast_forward`, §5.0); and the commit's OID is
  fixed by content-addressing, so it cannot be "replaced" by a stricter stub
  either (the §5.0.1 stub-swap trap, in reverse).

So disclosure is **monotonic / forward-only in tier as well as in topology**: a
state's effective tier may only ever move *more open* over time. An implementation
must reject (or at most warn-and-no-op) a `StateVisibility` record that would lower
the tier of a state already served to a broader audience — the sidecar can *record*
the intent for audit, but it has no power to recall distributed bytes. (Genuine
removal of already-distributed content is the separate, heavyweight `purge` path —
`OpRecord::Purge`, §2.3 — which is destructive and out of scope for a visibility
tier.) This makes the reviewer→public and embargo→public transitions safe and the
public→embargo transition structurally impossible, closing the "can a tier round-trip?"
question before it is asked.

**Every promotion is a persisted monotonic fact, never a recomputed predicate —
the durability mechanism that makes one-way hold across clocks and hosts.** (This is
the *single-host* face of the **persisted-fact principle** stated at the close of
this section, which the anchor (§5.3 rule 0) and any future placement/visibility fact
also instantiate.) The
one-way rule above is only as strong as the inputs the serve-time decision reads.
If visibility were recomputed from a **mutable** input — wall-clock
(`embargo_until`), per-host config, or any re-evaluated predicate — a clock skew, a
rolled-back clock, a config drift, or a second serve/export host could compute
`private` *after* another host already served the state `public`, silently
re-embargoing already-distributed bytes (exactly the §5.4 violation). The invariant
that forecloses this:

> **Every visibility promotion — manual `promote`, the reviewer-PR `set` (§7.2),
> the scheduled `embargo_until` lapse, and any future trigger — becomes a
> persisted, monotonic `StateVisibility` record (+ `OpRecord`, §5.5) at the moment
> it is first served to a broader audience. The serve-time visibility decision
> reads that persisted record (the latest non-superseded record,
> `RedactionsBlob::latest`-style, `redaction.rs:174`), and **never** recomputes
> public-vs-private from a mutable input (wall-clock, per-host config, a recomputed
> predicate).**

Inherited defaults (§8.1) are evaluated **once, at capture** — **not** recomputed at
first serve. Deferring the resolution to first serve would open a window in which a
thread/`[namespace]`/repo default drifting **more-open** between capture and the first
serve silently exposes an already-captured-but-not-yet-served private commit. **Invariant
A (§5.4 — immutable-at-capture)** closes that window: the default-resolution chain runs
at capture / visibility-record creation, and any resolution **more restrictive than
public** is persisted *then* as the state's initial `StateVisibility` record (§5.1). The
bound tier is **immutable** thereafter — a later change to any input can neither lower it
(re-embargo, forbidden above) nor raise it; default drift governs only states captured
*after* it. The "a config default that drifts more-open does **not** retroactively
promote an already-captured state" guarantee is therefore a **corollary** of Invariant A:
the only tier-raise is an explicit, signed, audited promotion record, and the only tier
value a default ever supplies for a state is the one frozen at that state's capture.

**Multi-host coordination — propagate promotion records *before* a host gates
(impl requirement, closes cid 3326047819).** (This is the *multi-host* face of the
**persisted-fact principle** below: it forbids gating — and, identically, anchoring —
from an un-propagated or lagging fact set.) A promotion record is the authority
that makes `S` servable to a broader audience, so it must reach a secondary
serve/export host **before** that host can correctly gate `S`. Shipping it via the
§8.4 audience-gate would be **circular**: §8.4 withholds a record until *its own
state* is served to the audience, but a lagging host cannot serve `S` until it
holds the promotion record, and cannot receive the record until it serves `S` — so
a host that still sees `S` as private would never learn the superseding `public`
fact and would keep gating a now-public state as private, **re-hiding** it from its
serve (a §5.4 violation in the multi-host plane). The fix is an explicit
ordering — **propagate/confirm, then gate**:

1. **The §8.4 gate is client-facing; it is not the host-to-host channel.** §8.4
   governs what a host serves to an under-tier *puller* — it stops an *embargo*
   (still-private) record from betraying a hidden commit's existence. A peer
   serve/export host is **not** an under-tier audience; it is a replica under the
   host-to-host trust list (a peer's signed record is honored iff its key is
   trusted, §8.4). Authoritative visibility records — the **promotion** records
   especially — replicate host-to-host as facts, **not** behind the audience gate
   that hides them from clients.
2. **Promotion records propagate ahead of (or together with) the bytes they
   authorize, and a host confirms it holds the authoritative record set before it
   applies gating.** A host gates a request only against the promotion facts it has
   authoritatively received; the materialized `public` record (+ `OpRecord`) lands
   on a peer **before** — never after — that peer fields a serve that could observe
   `S`. This rides the same propagation path redactions already use: a signed
   sidecar record travels alongside the objects during sync and the receiver
   verifies signature + trust list and persists it verbatim
   (`crates/client/src/grpc_hosted/sync.rs:268-302`); the `StateVisibility`
   promotion record replicates by the identical mechanism, just ordered **before**
   the receiving host gates.
3. **A host missing the authoritative records must fail toward last-known-public,
   never re-hide.** If a host detects it may be lagging (it cannot confirm it holds
   the records a peer may hold), it must **not** gate in the unsafe direction — it
   does not recompute `S` as private and re-hide it. It either **withholds serving**
   until it has synced the records, or **serves from its last-known-public state** —
   but it **never silently re-embargoes** a state that may already be public
   elsewhere. Re-hiding is the one §5.4-forbidden direction, so the failure mode is
   biased toward disclosing an already-public fact, never toward re-embargo.

A non-authoritative host still **must not** fire a schedule from its own clock or
recompute the tier locally (§5.4 monotonic-fact rule); it serves strictly from the
persisted promotion records it has confirmed it holds. So the implementation must
(a) designate the single authoritative host that fires `embargo_until` (the weft
serve host, O5), (b) **propagate each materialized promotion record to every other
host, and confirm its presence there, before that host gates `S`**
(record-before-gate, not record-after-serve), and (c) make a host that lacks the
authoritative records fail toward last-known-public — so no lagging or clock-skewed
host can serve a state at a stricter tier than the one already served elsewhere.

**The multi-host-consistency model — three invariants + propagate-before-use (§5.4).**
Every multi-host hazard in this design is foreclosed by **exactly four** named
guarantees, each stated **once** here and never re-derived per mechanism. They are
**orthogonal axes** — A fixes *when* a fact is bound, propagate-before-use fixes *that*
a host waits for the propagated fact before acting, B fixes *how* genuinely-concurrent
conflicting facts converge, and C fixes *how* served states are named so addressing
never collides — and every concrete mechanism (the tier promotion, the antichain
anchor, the synthetic ref, any future placement/visibility fact) routes to exactly one
of them rather than restating its own rule:

- **Invariant A — immutable-at-capture (the *when*).** A state's resolved visibility
  tier is **bound at capture** (visibility-record creation) and **immutable**
  thereafter. The inherited-default chain (§8.1) runs once, at capture; any resolution
  more restrictive than public is persisted then as the state's initial
  `StateVisibility` record (§5.1). Later drift of any input — thread / `[namespace]` /
  repo default — mutates **no existing state's tier**; it governs only states captured
  *after* the drift. (A public resolution needs no record: absence ≡ public, and the
  one-way constraint above forbids a later private-drift from re-embargoing it — so
  binding at capture is load-bearing precisely for the *restrictive* resolutions,
  which is where the exposure hazard lives.)

- **Invariant B — deterministic conflict-free merge (the *how-converge*).** When
  concurrency produces two conflicting facts for the same target with **no natural
  superseding order** — two hosts each legitimately minting a *first* fact (a
  thread/audience anchor, an own-line placement, any placement fact) before either has
  synced the other, so neither supersedes the other — resolution is a **deterministic
  merge on a content-intrinsic total order**, evaluated identically on every replica,
  so all converge on the **same** winner **without coordination**. The order is the
  **same key r15 fixed for initial anchor selection**: the candidate's `ChangeId`,
  least by raw byte order (`ChangeId` is `[u8;16]` deriving `Ord`, `hash.rs:98-99`).
  The losing fact is **superseded by this merge rule** — not by the monotonic-
  superseding-record rule, which cannot order anchors (they carry no more-open
  relation). This is heddle-native, CRDT-style convergence: a pure, idempotent
  `min`-over-identity join that needs no external state. *(Rejected alternative: a
  lease / single-writer lock on first advertisement. It converges too, but it demands
  coordination infrastructure heddle has no substrate for plus a liveness dependency
  on the lock holder; the intrinsic-key merge needs neither, so it is preferred.)* The
  key thus does **double duty** — `min`-selection within one host's antichain (the
  initial-anchor rule, §5.3 rule 0) *and* conflict-free merge across concurrent anchor
  facts.

- **Invariant C — collision-proof naming (the *how-name*).** Every synthetic name or
  key that must **uniquely address** a served state uses a **collision-proof
  encoding** — the **full** `ChangeId` (`ChangeId::to_string_full()`, `hash.rs:129`;
  round-trips via `parse()`, `:143`), **never** the truncatable `short()` / `Display`
  form (`:137`, `:167-170`) that two distinct `ChangeId`s could share. Synthetic refs
  are `refs/heads/<thread>@<full-changeid>`, so two maximal served siblings with
  distinct `ChangeId`s always map to distinct `RefEntry` / ref names; the multi-root
  advertisement (§5.3) therefore guarantees every maximal served state is **uniquely
  named *and* fetchable** — no frontier root is overwritten or made undiscoverable by
  a prefix collision.

- **Propagate-before-use — the persisted-fact principle (the *that-it-waits*).** A
  decision about **what** is served (a tier promotion) or **where** a moving ref sits
  (the antichain anchor, any future placement fact) is **computed once, by the
  authoritative writer**, persisted as a **signed, monotonic fact**, and **propagated
  before any host uses it**. Every other host **reads** that fact and **never
  re-derives the decision from local state** — neither from a *mutable input*
  (wall-clock, per-host config, a re-evaluated predicate) nor from a *lagging or
  differently-synced fact set* (a partial antichain, an un-received record). A host
  that cannot confirm it holds the fact **defers, or fails toward the already-public /
  already-chosen direction** — it withholds or serves last-known-public and never
  re-hides; it defers a first advertisement rather than minting a second anchor. (Its
  *single-host* face is "no recompute from a mutable input"; its *multi-host* face is
  propagate-before-gate, "no gating — and no anchoring — from an un-propagated /
  lagging fact set." Invariant A supplies the binding moment this principle reads from;
  Invariant B resolves the one case propagate-before-use leaves open — when two writers
  genuinely race and there is no single writer to wait for.)

The **one** value ever *computed* rather than *read* is a decision's initial value at
the moment of its **capture** — the inherited tier default (§8.1), frozen by Invariant
A into a persisted fact **at capture**, after which only the fact is read. There is no
fact to read before the first writer mints one, which is exactly why only the writer
computes; genuinely-concurrent writers converge by Invariant B. All four guarantees
converge on the same §5.4 outcome — once a decision is bound, no clock, config drift,
replica holding a different fact set, concurrent minter, or duplicate name can walk it
back, fork it, or hide a served state behind a colliding name.

### 5.5 Oplog records (append at the tail — hard constraint)

`OpRecord` is encoded by discriminant index and **new variants must append at the
tail** (`crates/oplog/src/oplog/oplog_types.rs:14-21`). Add, at the tail:

- `StateVisibilitySet { visibility_id: ContentHash, state: ChangeId, tier: VisibilityTier }`
- `StateVisibilityPromote { visibility_id: ContentHash, state: ChangeId, from: VisibilityTier, to: VisibilityTier }`

mirroring the `Redact` / `Purge` audit-trail pattern (`oplog_types.rs:109,123`).

---

## 6. Data-model / API / schema implications (summary)

| Layer | Change | Grounding / precedent |
|---|---|---|
| `objects` crate | new `StateVisibility` + `StateVisibilityBlob` objects; (preferred) rename `AnnotationVisibility` → shared `VisibilityTier` **+ strictest `Private { scope_label }` variant** | mirrors `Redaction`/`RedactionsBlob` (`redaction.rs:29,133`) |
| `repo` filter | extend `visible()` with the `Private` arm **above** the all-seeing-`Internal` arm | `visible()` (`visibility.rs:148,153`) |
| object store | per-state `visibility/` sidecar dir + read/write + `has_visibility_for_state` | mirrors redactions dir (`fs_paths.rs:46-50`, `repository_redaction.rs:490`) |
| `oplog` | tail-append `StateVisibilitySet` / `StateVisibilityPromote` | tail-append rule (`oplog_types.rs:14-21`) |
| `repo` resolve | thread `AudienceTier` through the checkout entry (above `materialize_tree`, `:299`); the **operator-local courtesy stub** is rendered at the **state walk** (where the `ChangeId` is in scope), never in blob-keyed `materialize_blob` (`:575`, no `ChangeId`/audience) — the public mirror emits absence, not a stub (§5.0/§5.3) | redaction stub (`repository_materialization.rs:591`), filter (`visibility.rs:148`) |
| bridge | add `AudienceTier` param to `export_state` (`:28`, holds the `ChangeId`) so minting is audience-aware; for the public mirror **compute the visibility frontier before *every* ref-publishing/state-emitting surface** via one shared `resolve_frontier` chokepoint (§5.3) — branch ref-sync (`:277-288`, lag `refs/heads/main` to the frontier, **not** the raw `get_thread` tip `:278`), marker→tag sync (`:290-296`, withhold `refs/tags/<marker>` unless served — conflict-not-FF, `git_sync.rs:163-170`), **state notes `refs/notes/heddle`** (write a note only for a served state — `git_export.rs:242-245,252-261`, `git_core.rs:1105-1109`; forced mirror `+refs/notes/*`, `git_core.rs:300`; note payload carries `change_id`/attribution/agent, `git_notes.rs:33-56`), and the **`HEAD` symref + bulk push** (`git_core.rs:2398-2407,751`); merge-frontier antichain ≥2 → `<thread>` advances along its own line to its prior tip's maximal served descendant; publish every *other* sibling line under deterministic `refs/heads/<thread>@<full-changeid>` synthetic refs; the frontier rule is categorical over all surfaces (§5.3), so the embargoed commit *and its descendants* are absent (forward-only, §7.1 step 3) — disclosure FF-appends, never re-mints/force-pushes (`ensure_commit_update_fast_forward`, `git_core.rs:2446`; mapping-skip `git_export.rs:218-223`); **no stub commit** (stub-swap/parent-reparent ruled out, §5.0.1) — not in `export_tree` (`:97`, tree-keyed, no audience) | per-state mint (`git_export.rs:84-93`), FF guard (`git_core.rs:2446`), notes (`git_notes.rs:29`) |
| `proto` / wire | new `ObjectType::Visibility` in the sync plan (mirroring `emit_redaction_plan`), itself gated — a record is served only when its state is served, so it never leaks an embargoed `ChangeId`/tier/date (§8.4); **new** tier-aware **downward-closed reachability gate** — resolve the visibility frontier as a pre-pass, then serve the forward closure of the ancestry-closed visible set rooted at that frontier (no `ObjectType::State` header for an embargoed commit); **multi-root merge frontier** — `ListRefs` advertises one `RefEntry` per maximal served state into the existing `Vec<RefEntry>` (`message_refs.rs:85`), client issues one single-root `Pull` per root (`message_pushpull.rs:91`) with `exclude_states` dedup (`:95`), so no widened request shape is needed. NOT a `collect_excluded` extension (root-exclusion over-withholds, §5.3) and no set difference is needed (a child of a hidden parent is never served, so nothing shared to subtract) | `emit_redaction_plan` (`object_graph.rs:346`); contrast `collect_excluded` (`:360`); `Vec<RefEntry>` (`message_refs.rs:85`) |
| weft (closed) | **authoritative** server-side downward-closed gate in `ListRefs`/`Pull` (above); grant-role → `AudienceTier` mapping; optional `PromoteVisibility` RPC + scheduler | `RepoSyncService` (`service.proto:8`); role substrate (`contribution-grant-flows.md` §1) |
| config | `[namespace.<name>] default_state_visibility` + repo-wide default; reuse the resolution *precedence pattern* (namespace → repo → fallback) — but `resolve_default_visibility` is typed to `AnnotationVisibility` (`namespace_policy.rs:68,75`), so it must be generalized over the tier enum (ties O4) and its `Internal` fallback replaced with the stricter `Private` (§8.1) | `resolve_default_visibility` (`namespace_policy.rs:68`) |

No change to `State` itself — its tail-append invariant and signatures are
preserved precisely because visibility lives in the sidecar.

---

## 7. Worked examples

### 7.1 Embargoed security fix

Setup: public thread `main`, mirrored to `refs/heads/main`. The thread's default
tier is `public` (resolved from config, §8). Commit `N` describes the exploit;
commit `N+1` is the fix. `N+1.parents = [N]` (by `ChangeId`, `state_core.rs:207`).

1. **Declare the embargo.** `heddle visibility set N --tier private:security --until
   2026-07-01T00:00:00Z --sign-with ops.pem`. This writes a `StateVisibility`
   record (`tier = Private { scope_label: "security" }`, `embargo_until =
   2026-07-01`) into `N`'s sidecar and an `OpRecord::StateVisibilitySet` (§5.5).
   The `Private` tier is withheld from every audience except a reader holding
   `restricted:security` (§5.2) — including the otherwise all-seeing `Internal`
   audience. `N+1` keeps the thread default (`public`).
2. **Public pull (hard) — the embargoed commit and its descendants are absent.**
   A reader with public tier calls `Pull`. The weft serve **pre-pass** resolves
   the served frontier for this audience (§5.3) — the last visible ancestor
   `N-1` — *before* any object is emitted, so the closure emission is rooted at
   `N-1` and `N` is never enumerated: it emits neither `N`'s `ObjectType::State`
   object — so none of `N`'s `intent` / `attribution` / `signature` / timestamp
   metadata travels, because there is no header-only form to serve (§5.0, closing
   cid 3324616942) — nor anything reachable only through `N`. Because `N+1` has
   `N` as an ancestor, `N+1` is **also** absent (downward-closure, §5.0/§5.3);
   the public `ListRefs` **lags** the thread ref to the last all-public ancestor
   `N-1` (it rewrites the ref's tip, never dropping the already-public ref —
   §5.3). The public clone learns nothing of `N` or `N+1` — not their content,
   not their metadata, only that the public tip is `N-1`.

   **Honest scope note.** A fix that *descends from* an embargoed commit therefore
   cannot reach the public mirror ahead of disclosure. The §1 goal — "ship `N+1`
   publicly while `N` stays embargoed" — holds **only when the embargoed material
   is not an ancestor of the published fix** (e.g. the exploit write-up/PoC is a
   *descendant* of the fix, or sits on a side branch); then the fix publishes and
   the write-up stays embargoed under the same gate. When the exploit commit
   genuinely precedes the fix on one line of history, both wait for disclosure on
   the public mirror, while authorized pullers (step 4) get the fix immediately
   over the wire. Trading this narrow capability away is the price of a sound
   embargo — the approaches that would preserve it (header-only, audience-scoped
   partial serialization) are the ruled-out traps of §5.0.1.
3. **Public Git mirror — the same gate, no stub.** The Git mirror is just the
   Git-projection of the step-2 gate: the bridge runs the same frontier pre-pass
   (§5.3) **before its ref-sync** (`git_export.rs:277-288`), finds the last
   all-public ancestor `N-1`, and points `refs/heads/main` at `N-1` — not at the
   raw `get_thread` tip (`git_export.rs:278`) — so `N` and its descendants `N+1…`
   are absent from the mirror. There is no way
   to do otherwise, because a Git commit's OID is the hash of its tree + parent
   OIDs (`export_state` → `new_commit_as(..., git_tree_oid, parent_oids)` →
   `commit.id`, `crates/cli/src/bridge/git_export.rs:84-93`; parent OIDs resolved
   from the persisted mapping `mapping.get_git(parent_id)`, `:62-70`) — publishing
   a descendant requires a *real* parent commit, and no stub stands in for `N`
   (stub-swap and parent-reparent are ruled out, §5.0.1). Cost (documented
   honestly): the public **Git** tip lags the private thread tip while the embargo
   holds; authorized pullers get `N`/`N+1` over the wire (step 4), the Git mirror
   gets them at disclosure (step 5).
4. **Reviewer/maintainer pull.** A caller whose grant maps to
   `AudienceTier::Restricted("security")` — the authorized embargo scope (§2.6) —
   pulls and sees `N` in full: same objects, no stub. No other tier, including
   `AudienceTier::Internal`, is admitted by the `Private` arm (§5.2).
5. **Disclosure — forward-only, never a swap.** Disclosure happens by one of two
   routes that converge on the **same persisted record**: either someone runs
   `heddle visibility promote N`, or on 2026-07-01 the authoritative host observes
   `embargo_until` has lapsed and **materializes** the promotion (§5.4). Both append
   a superseding `public` `StateVisibility` record + `OpRecord::StateVisibilityPromote`
   (§5.4/§5.5) *before* the first public serve; from then on the public tier is read
   from that record, never recomputed from the clock.
   `N` becomes visible to the public tier, so `N` **and** its now-eligible
   descendants `N+1…` enter the public served set together (the downward-closure
   gate is simply re-evaluated, §5.3). Disclosure is the **first** public export
   of `N`, not an edit of a published one: the next bridge run exports `N` from
   its **real** tree for the first time and fast-forwards `refs/heads/main` to
   `… → N-1 → N → N+1`. Each commit is minted exactly once from real content
   (`export_state` skips any state already in the mapping — `git_export.rs:218-223`
   — so a commit is never re-minted), and the new tip is a descendant of the old
   public tip `N-1`, so the FF guard passes (`ensure_commit_update_fast_forward` →
   `commit_is_descendant_of`, `crates/cli/src/bridge/git_core.rs:2280,2293`). No
   published OID changes; no force push.

   **Why the naïve "swap the stub for the real tree" is rejected — the two hard
   constraints (the basis of the §5.0.1 ruled-out traps).**
   - **(a) A commit's OID is a pure function of its content + parents.**
     `export_state` mints the commit via `repo.new_commit_as(sig, sig, message,
     git_tree_oid, parent_oids)` and returns `commit.id`
     (`crates/cli/src/bridge/git_export.rs:84-93`); the tree OID (`:41`, `:89`) and
     parent OIDs (`:62-70`, `:90`) are inputs to that hash. Replacing the stub tree
     with the real tree changes `git_tree_oid` → changes `N`'s OID → changes the
     parent OID that every descendant (`N+1`…tip) feeds into its own
     `new_commit_as`, re-minting the entire chain.
   - **(b) The public mirror is fast-forward-only.** `sync_track_to_branch` guards
     every branch update with `ensure_commit_update_fast_forward`
     (`crates/cli/src/bridge/git_sync.rs:134`), which rejects any new tip that is
     not a descendant of the published tip (`NonFastForwardRef`,
     `crates/cli/src/bridge/git_core.rs:2446-2466`) — there is no force path in this
     code. A re-minted chain is not a descendant of the published chain, so the swap
     could only land as a non-fast-forward *rewrite* of a published mirror
     (unacceptable). And even attempting it is moot: the mapping-skip
     (`git_export.rs:218-223`) means a re-export keeps the stub OID and never mints
     the real tree at all — so "replace on next export" silently never happens.

   Disclosure is therefore forward-only by construction, as above.

### 7.2 In-flight PR with a reviewer audience

Setup: a feature thread `feat/x`. Its default tier is `private:<author-scope>` —
withheld from every audience but the author until they open it for review; nothing
reaches a public puller before merge.

1. **Open for review — raise the *whole ancestry*, not just the tip.** Because the
   thread default is `private:<author-scope>` and the downward-closed gate (§5.3)
   serves a state only when **every ancestor** is visible to the same audience,
   raising only the tip to `reviewers:secteam` leaves the multi-commit thread
   **unfetchable** — the still-`private` ancestors keep the reviewer-facing tip
   withheld, so the reviewer's `Pull` returns nothing. The open-for-review step
   must therefore raise **every state in the thread's ancestry** (back to the
   nearest ancestor already visible to `secteam` — e.g. the public merge-base) to
   at least `reviewers:secteam`, per the ancestry-transitive promotion rule
   (§5.4). Two routes:
   - **Set the default before capture** (cleanest): set the thread's default tier
     to `reviewers:secteam` *before* the commits land — precedence layer 2 of the
     inherited-default resolution (§8.1) — so every commit is born at the reviewer
     tier and the ancestry is uniformly visible with no later raise.
   - **Promote an already-captured thread transitively:** apply
     `heddle visibility set @ --tier reviewers:secteam --all-states` (§8.2). The
     `--all-states` flag is carried from `redact` (`commands_redact.rs:101`),
     where it walks `reachable_states()` (`redact.rs:132-134`); for PR promotion
     it must scope to the tip's ancestry (the reviewer needs the ancestors of `@`,
     not every reachable state in the repo).

   `reviewers:secteam` → `Restricted { scope_label: "secteam" }`, which — unlike
   `private:` — is also visible to the internal trusted set so CI/tooling can read
   the proposed commits (§5.2). One `StateVisibility` record lands per raised
   state. (A single-commit thread is the degenerate case where "the tip" and "the
   whole ancestry" coincide — but the design must not assume it; §5.4.)
2. **Reviewer fetch.** A reviewer holding the `secteam` audience (mapped from a
   per-thread grant → `AudienceTier::Restricted("secteam")`, §2.6) calls
   `ListRefs`/`Pull`; the tier predicate admits the thread and its states. They
   review the proposed commit.
3. **Public is blind.** A public/anon caller's `ListRefs` omits `feat/x`
   entirely (tip tier above public). No ref, no objects.
4. **Merge.** On merge, `heddle visibility promote` the merged states to `public`
   (or the merge into a public thread inherits that thread's public default), and
   the bridge exports them normally.

---

## 8. CLI surface — justified on long-term ergonomics

Design priority: **sensible defaults over flag-proliferation; composable, not
niche.** The shipped redaction family (§3) is the size and shape to match.

### 8.1 Defaults do the work; the flag is the exception

A per-commit visibility flag on `capture`/`snapshot` would be flag-proliferation
of the worst kind (every commit asks "who can see this?"). Instead, tiers
**inherit**, resolved through the same precedence *pattern* as the annotation
resolver (`namespace_policy.rs:68`, today typed to `AnnotationVisibility` — reuse
means generalizing it over the tier enum, O4):

1. explicit `heddle visibility set` on the state (the deliberate exception),
2. the thread's default tier,
3. `[namespace.<name>] default_state_visibility` in config,
4. repo-wide default,
5. hard-coded fallback — **fail-closed: the strictest tier, never public.** This
   mirrors the *pattern* of the annotation resolver, which falls back to
   `AnnotationVisibility::Internal` (`namespace_policy.rs:76`) as its "we don't
   know who should see this" choice — but for commits the fallback must be
   *stricter* than `Internal`, which leaks to team/internal audiences (§5.2).
   Because the strictest `Private` tier carries an authorized scope label, the
   repo-wide default must name that scope; resolving the label for a bare
   fail-closed default is open question O8.

So a normal public thread sets its default once; every commit is public without a
flag. The embargoed-fix case is the *only* time you reach for `visibility set`,
to mark the one exceptional commit. This is the whole ergonomic argument: the
common path has **zero** new flags; the rare path has one verb.

This resolution runs **at capture**, not at first serve, and binds the tier
**immutably** (Invariant A, §5.4): the chain above is evaluated once when the state is
created, and a resolution more restrictive than public is persisted then as the
state's initial `StateVisibility` record — so a default later drifting more-open never
retroactively exposes an already-captured state. The "zero new flags" ergonomic is
about the *CLI surface* (you never type `--visibility` per commit), not about deferring
the decision: a public resolution still needs no stored record (absence ≡ public), but
a restrictive one is pinned at capture, which is exactly where the exposure hazard
would otherwise live.

### 8.2 The verb family (minimal, mirrors `redact`)

```
heddle visibility set <state> --tier <public|reviewers:LABEL|private:LABEL> [--until RFC3339]
                              [--all-states] [--sign-with PEM] [--sign-algo A]
heddle visibility promote <state>            # supersede with a more-open tier now (audited)
heddle visibility show <state>               # the effective tier + record chain
heddle visibility list                       # every non-default tier in the repo
```

Justification, verb by verb (none is "for completeness"):

- **`set`** — the irreducible declaration. Someone must mark the exception; it
  cannot be defaulted away. `--tier` is a single enum value (not one flag per
  tier). `reviewers:LABEL` (→ `Restricted`) and `private:LABEL` (→ the strictest
  `Private`) both name an authorized scope but differ in one arm: `reviewers:` is
  also visible to the internal trusted set (`AudienceTier::Internal`), `private:`
  is withheld even from it (§5.2) — pick `private:` for a hard embargo, `reviewers:`
  for an in-flight PR that internal CI/tooling may read. `--until` folds the
  scheduled-promotion case into `set` instead of a separate "schedule" verb. `--all-states`, `--sign-with`, `--sign-algo` are
  carried verbatim from `redact apply` (`commands_redact.rs:87-111`) for muscle
  memory and because the embargo declaration wants the same signing story.
- **`promote`** — a *distinct* verb, not `set --tier public`, because opening an
  embargo is the auditable lifecycle moment (its own `OpRecord`,
  `StateVisibilityPromote`) and is the manual analogue of the `--until`
  scheduler. Conflating it with `set` would lose the "this was deliberately
  revealed, by whom, when" signal.
- **`show` / `list`** — inspection, one-to-one with `redact show` / `redact list`
  (`commands_redact.rs:24-26`).

### 8.3 The reader side reuses existing grammar

"Operate as audience X" (clone/fetch, and the planned `bridge git export
--audience`) reuses the **already-shipped** `--audience
internal|public|team:NAME|restricted:LABEL` grammar (`visibility.rs:60-87`)
rather than minting a parallel selector. No new reader-side vocabulary: an
authorized embargo/reviewer holder reads as `restricted:<label>`. Note that with
the new `Private` arm (§5.2), `--audience internal` no longer sees *everything* —
it sees every tier except `Private`, which is admitted only by the matching
`restricted:<label>`.

### 8.4 Wire-trust reuse

Visibility records that propagate over the wire are governed by the same
**fail-closed trust list** model as signed redactions (`RedactTrustCommands`,
`commands_redact.rs:41`): a peer's signed visibility record is honored only if
its key is trusted, so a malicious peer cannot *forge* a more-open tier. (The
*hard* boundary does not depend on this — it's the server withholding bytes; the
trust list governs the records that ride alongside.)

**The record is gated by the same downward-closure as its state (so it is not a
back-door leak).** A `StateVisibility` record names the state's `ChangeId`, tier,
`embargo_until`, and declarer — metadata that itself would betray the existence of
an embargoed commit. So a visibility record for state `S` is served to audience
`A` **only when `S` is served to `A`** (the §5.3 gate), never alongside the gap a
withheld `S` leaves. A public puller therefore sees neither the embargoed state
*nor* the record announcing it — closing the obvious sibling of the header leak.

**This gate is *client-facing* only — it does not govern host-to-host
replication.** It decides what a serve host hands an under-tier *puller*; it is
**not** the channel by which one authoritative serve/export host replicates records
to another. A peer host is a trusted replica, not an under-tier audience, so
authoritative records — **promotion** records above all — propagate host-to-host as
facts *before* the receiving host applies this client-facing gate (§5.4
"propagate-before-gate"). Reading this §8.4 gate as the host-to-host transport
would be circular — a lagging host could never learn the `public` fact that lets it
serve `S` — which §5.4 explicitly forecloses.

---

## 9. Open questions

- **O1 — where the hard filter lives.** The authoritative serve-side withhold is
  in the **closed weft** repo (`RepoSyncService` handlers), not this workspace.
  This spike specifies the OSS-side object, records, wire-plan exclusion, and
  cooperative render; the weft filter + the grant→tier mapping are a cross-repo
  follow-up (issue 4, §10). Confirm the split with the maintainer.
- **O2 — caller-tier derivation.** How does a caller's `AudienceTier` come from
  the auth context? Likely a **per-thread grant** evaluated via the
  `GetMyEffectiveRole` RPC proposed in `contribution-grant-flows.md` §3.2/§4
  (applied with a thread as the target resource) rather than a global role.
  Is "reviewers" a role, a per-thread grant, or a named `Restricted { label }`
  audience? Recommendation: per-thread grant carrying an audience label.
- **O3 — bridge DAG strategy (RESOLVED: downward-closed non-publication).** The
  public Git mirror is fast-forward-only (`ensure_commit_update_fast_forward`,
  `git_core.rs:2446`) and a commit's OID is fixed by its tree + parents
  (`export_state`, `git_export.rs:84-93`), so a published commit's identity can
  never change. **Resolution:** the bridge runs the same frontier pre-pass as the
  wire surfaces **before its ref-sync** (`git_export.rs:277-288`, §5.3) and lags
  `refs/heads/main` to the last all-public ancestor (the frontier) rather than the
  raw `get_thread` tip (`:278`); minting is audience-aware too, but the ref tip is
  decided by the frontier, never the raw tip, so the embargoed commit **and its
  descendants** are absent. Disclosure FF-appends the real commits with true OIDs
  (§5.0/§7.1 step 3/step 5).
  This is the Git-mirror projection of the one downward-closed gate (§5.3) — no
  separate strategy. The same pre-pass governs **all** Git ref-publishing
  surfaces, not just the branch ref: the marker→tag sync, the `refs/notes/heddle`
  note writes (a **history-bearing** auxiliary ref — **rebuilt on withhold**, not
  merely tip-filtered, since its parent chain accretes every past note, §5.3), the
  `HEAD` symref, and the bulk export/push (§5.3 surfaces 4–6, §10 #5). And a
  hidden merge that leaves ≥2 incomparable maximal served states is
  handled by the multi-root path — `ListRefs` advertises the antichain as multiple
  `RefEntry`s and the Git mirror publishes synthetic `<thread>@<full-changeid>`
  refs (§5.3) — so no visible side is silently unreachable. The earlier "permanent
  stub commit" fallback is **dropped**: it would publish the descendant against a
  synthetic stub parent (a partial embargoed-commit view + a parent-edge rewrite),
  which §5.0.1 rules out. Stub-swap and graft-reparent `N+1` onto `N-1` are
  likewise rejected (both change a published OID → non-FF rewrite; break change-id
  stability + signatures). No remaining sub-question — there is one strategy, not
  an A-vs-B choice.
- **O4 — enum unification + new tier.** Promote `AnnotationVisibility` → a shared
  `VisibilityTier` across annotations/discussions/states (recommended; already
  reused by two consumers) and add the strictest `Private { scope_label }` variant
  + its `visible()` arm above the all-seeing-`Internal` arm (§5.2), vs a separate
  `StateAudience` (more isolation, more duplication of the `visible()` table). The
  `Private` arm changes `visible()` for *all* consumers (annotations/discussions
  could use it too) — confirm that's acceptable, or scope the arm to states.
- **O5 — clock trust for `embargo_until` (RESOLVED: advisory trigger →
  materialized persisted fact).** `embargo_until` is **not** a serve-time predicate
  any host re-evaluates from its clock. It is an advisory schedule that the **single
  authoritative serve host** (the weft serve host) fires: when its clock first
  reaches `embargo_until` it materializes a superseding `public` `StateVisibility`
  record + `OpRecord::StateVisibilityPromote` (§5.4) *before* the first public serve.
  Every host — including a skewed/rolled-back one or a lagging second host — then
  reads the tier from the **persisted record** (§5.1), never from its own clock, so a
  fired-and-served promotion can never be recomputed back to `private`. A
  client-evaluated clock is rejected outright. Multi-host propagation is
  **propagate-before-gate** (§5.4): the materialized record replicates host-to-host
  (over the redaction-style record-sync path, not the §8.4 client gate) and is
  confirmed present on a host **before** that host gates the state; a host that
  cannot confirm it holds the authoritative records **fails toward last-known-public
  and never re-hides** — so the missing-record case never re-embargoes a state
  already served elsewhere. Impl requirement (§5.4, issues #4/#5).
- **O6 — signature scope.** A `State.signature` signs the state bytes; the
  visibility sidecar is outside that, so an embargo declaration needs its **own**
  signed payload (mirror `Redaction::canonical_signing_payload`,
  `redaction.rs:67`). Confirm the canonical payload fields for `StateVisibility`.
- **O7 — header-visible vs fully-withheld (RESOLVED: fully-withheld).** An
  under-tier state is withheld **whole**; no "header" travels. There is no
  header-only `State` form to serve: the wire emits one `ObjectType::State` object
  whose payload is `rmp_serde::to_vec_named(&state)` over the entire `State`
  (`object_graph.rs:84-90`), which carries `intent`/`attribution`/timestamps/
  `verification`/`signature` (`state_core.rs:202-214`) — so a "header" *is* the
  metadata leak (finding cid 3324616942). Because the child of a withheld state is
  itself withheld (downward-closure, §5.0), no consumer ever needs the embargoed
  parent's header to resolve a served child's DAG edge — the served set is
  ancestry-closed, so every served state's parents are also served.
- **O8 — fail-closed default's authorized scope.** The strictest `Private` tier
  carries a `scope_label`, but the last-resort fallback (§8.1 step 5) fires when no
  default is configured — so there is no label to fall back to. Options: require the
  repo-wide default to name an owner/admin scope (so the fallback is
  `Private { scope_label: <owner> }`), or have the fallback withhold from *every*
  audience (a `Private` with no admitting arm) until an operator classifies the
  state. Confirm which, since "withhold from everyone" can wedge a fresh repo.

---

## 10. Proposed follow-up implementation issues (NOT filed — for maintainer triage)

Per spike discipline, these are proposed only; the orchestrator confirms scope
before filing.

**Coverage contract (so nothing is left uncaptured for the implementer).** The
issues below are the maintainer-facing work list, so they must enumerate *every*
surface and *every* structural dimension surfaced across review rounds r2–r9 — a
reviewer must not be able to say "X isn't captured." The complete inventory the
issues are checked against:

- **Ref-publishing / state-emitting surfaces (six, §5.3, exhaustive as of this
  audit):** (1) wire closure planner (`object_graph.rs:59`); (2) `ListRefs` —
  including the **multi-root antichain advertisement** via the `Vec<RefEntry>`
  (`message_refs.rs:85`); (3) Git-bridge branch ref-sync (`git_export.rs:277-288`);
  (4) Git-bridge marker→tag sync (`git_export.rs:290-296`, `git_sync.rs:157`); (5)
  Git-bridge state notes `refs/notes/heddle` — the lone **history-bearing** surface,
  **rebuilt on withhold** (not just tip-filtered) (`git_notes.rs:29`; writes at
  `git_export.rs:242-245,252-261` + `git_core.rs:1105-1109`; `write_note` parents on
  prior head `git_notes.rs:180,195-204`; forced mirror `+refs/notes/*`,
  `git_core.rs:300`); (6) the `HEAD` symref (`git_core.rs:2398-2407`)
  + bulk `export_to_path`/`push` (`git_core.rs:751,678-689`). Plus the structural
  close-the-class fix: one shared `resolve_frontier` chokepoint + a conformance
  test that no ref-publishing or note-writing call site takes a raw
  `get_thread`/`get_marker`/`mapping`-iterated OID.
- **Structural dimensions (§5.3/§5.4):** linear frontier; merge-DAG frontier as an
  antichain/cut; **octopus** merges (>2 parents); **criss-cross / multiple merge
  bases** (gate selects no base); frontier **computation** (least-fixed-point
  topological pass) and **transmission** without leaking (absence ≡ non-existence);
  the **multi-root protocol path** (advertise antichain, N single-root pulls,
  synthetic Git refs); **antichain ref-placement stability** — the moving ref
  (`<thread>` / `refs/heads/<thread>`) is stable + forward-only across antichain
  members: it advances forward **along its own line** to the maximal served
  descendant of its prior tip (e.g. `A → A2`, never frozen at a non-maximal
  ancestor), else holds its prior tip, and **never moves laterally to a sibling
  member or regresses** (no ordering-based selection of the moving ref, §5.3); **cross-path disclosure
  ordering** (forward-only under every interleaving); **transitive promotion** up
  all parent paths; the **one-way tier constraint** — no re-embargo of a served
  state; and **ref
  history-reachability** — single-target/frontier-governed refs (branch, tag,
  `HEAD`, wire surfaces: filter/lag/withhold suffices) vs **history-bearing
  auxiliary refs** (state notes `refs/notes/heddle`, and any `+refs/notes/*`) that
  must be **rebuilt on withhold** so no embargoed object is reachable through the
  published ref's history (§5.3); and **persisted-monotonic promotion** — every
  visibility promotion (manual `promote`, reviewer `set`, scheduled `embargo_until`)
  is a **persisted `StateVisibility` record read at serve time, never recomputed
  from a mutable input** (wall-clock / config / per-host), with the scheduled case
  *materialized before first serve* by the single authoritative host and
  *propagated across hosts*, so clock skew / a rolled-back clock / a second serve
  host cannot re-embargo an already-served state (§5.4 durability rule); and
  **promotion-record propagation/coordination (propagate-before-gate)** — a host
  gates `S` **only** on promotion facts it has authoritatively received: promotion
  records replicate host-to-host (not behind the §8.4 client gate, which would be
  circular) and are confirmed present *before* the host gates, and a host missing
  the records **fails toward last-known-public, never re-hides** (§5.4 multi-host
  rule); and the **three named multi-host-consistency invariants (§5.4)** the whole
  section rests on, each with its own conformance check: **A — immutable-at-capture**
  (the inherited-default chain resolves **once at capture**, persists any
  restrictive resolution as the initial `StateVisibility` record, and is **never**
  recomputed at first serve, so default drift cannot expose an already-captured
  state — issue #3); **B — deterministic conflict-free merge** (two genuinely-
  concurrent first-advertisement / placement facts with no superseding order converge
  by a content-intrinsic `min`-`ChangeId` merge, identical on every replica, **no
  lease / single-writer** — issue #4); **C — collision-proof naming** (every synthetic
  ref/addressing name uses the **full** `ChangeId` via `to_string_full()`, **never** a
  truncatable `short()`/prefix form, so two distinct siblings never collide and every
  maximal served state is uniquely named *and* fetchable — issues #4/#5).

1. **impl(objects/repo): `StateVisibility` object + per-state sidecar store.**
   Add `StateVisibility` / `StateVisibilityBlob` (objects), the `visibility/`
   sidecar dir + read/write + `has_visibility_for_state` (repo/store), modeled on
   `Redaction`/`RedactionsBlob`. Blocked by this spike.
2. **impl(repo/bridge): audience plumbing + operator-local courtesy stub.** Extend
   `visible()` with the strictest `Private` arm (above the all-seeing-`Internal`
   arm); thread an `AudienceTier` through the checkout entry (above
   `materialize_tree`) and through `export_state` (which already holds the
   `ChangeId`). The bridge enforces the public mirror by **computing the visibility
   frontier before its ref-sync** and lagging `refs/heads/main` to that frontier
   (issue #5), emitting absence. The render-stub is
   only the **operator-local checkout courtesy** at the **state-walk level** — *not*
   the blob-keyed `materialize_blob`/`export_tree` (no `ChangeId`/audience), and
   *not* a public-mirror surface (§5.0/§5.3). Blocked by #1.
3. **impl(oplog/cli): tier records + `heddle visibility` verb family.**
   Tail-append `StateVisibilitySet`/`StateVisibilityPromote`; implement `set` /
   `promote` / `show` / `list`; wire the config-default resolution chain
   (`namespace_policy.rs:68`, generalized over the tier enum). **Invariant A
   (immutable-at-capture, §5.4): resolve the inherited default *at capture*, not at
   first serve** — run the chain once when the state is created and **persist any
   resolution more restrictive than public** as the state's initial `StateVisibility`
   record (a public resolution stays record-free: absence ≡ public). This binds the
   tier immutably at capture so a `[namespace]`/repo default later drifting more-open
   cannot retroactively expose an already-captured-but-not-yet-served state. A
   conformance test must assert: capture a state under a restrictive default, drift
   the default to public, and verify the captured state's effective tier is
   **unchanged**. Blocked by #1.
4. **impl(weft, cross-repo): authoritative serve-side downward-closed gate +
   multi-root advertisement.** Gate `ListRefs`/`Pull` by caller tier via the
   **tier-aware downward-closed reachability pass** (§5.3): resolve the visibility
   frontier as a pre-pass (the least-fixed-point topological computation, §5.3),
   then serve the forward closure of the ancestry-closed visible set rooted at that
   frontier, so no `ObjectType::State` header for an embargoed commit (or any
   descendant) is emitted. *Not* an extension of the root-exclusion `collect_excluded` (which
   over-withholds blobs a visible child shares with a hidden parent), and no set
   difference is needed — a child of a hidden parent is never served, so there is
   nothing shared to subtract. **Merge-DAG handling is in scope:** the frontier is
   an antichain (handles octopus >2-parent merges and criss-cross/multiple merge
   bases — the gate selects no merge base); when the antichain has ≥2 incomparable
   maximal served states, **`ListRefs` advertises one `RefEntry` per maximal served
   state** into the existing `Vec<RefEntry>` (`message_refs.rs:85`): `<thread>`
   **advances along its own line to the maximal served descendant of its prior
   tip** (never frozen at a non-maximal ancestor) and **only the *other* members**
   (sibling lines) get deterministic `<thread>@<full-changeid>` names — **Invariant C
   (collision-proof naming, §5.4): the name carries the full `ChangeId` via
   `ChangeId::to_string_full()` (`hash.rs:129`), never a truncatable `short()`/prefix
   form (`:137`/`:167-170`)**, so two siblings with distinct `ChangeId`s never collide
   onto one `RefEntry`. `<thread>` itself is **never** chosen by topological/
   lexicographic order and **never** jumps to a sibling (that would move the main ref
   sideways to an incomparable sibling, §5.3 "antichain ref placement, the definitive
   statement"). A conformance test must assert two siblings sharing any `ChangeId`
   prefix still receive distinct ref names and both remain fetchable. The client issues **one
   single-root `Pull` per root** (`PullRequest.target_state`, `message_pushpull.rs:91`)
   carrying prior states in `exclude_states` (`:95`) — so every visible side is
   discoverable and requestable without widening the single-`ChangeId` request
   (§5.3 "protocol path for all merge-frontier roots"). **Antichain ref-placement
   stability is a conformance requirement:** assert the advertised `<thread>` tip is
   forward-only across re-advertisements — it advances only to a served descendant
   **of its own prior tip** (its own-line maximal descendant), never to an
   incomparable sibling, and never regresses. **Initial anchor is deterministic +
   host-independent:** when the thread's first advertisement at an audience is already
   an antichain (no prior tip), the **first advertiser** computes `<thread>`'s own line
   as the member with the **byte-order-least `ChangeId`** (`hash.rs:98-99`) **once**,
   writes it as a fact over the record-sync path (`sync.rs:268-302`), and every
   **other** host **reads** that fact — a host lacking it **defers** rather than
   recomputing from its own (possibly lagging) antichain — so every host adopts the
   same member, never re-selected from a mutable/lagging antichain (propagate-before-use,
   §5.4; §5.3 "definitive statement," rule 0). **Invariant B (deterministic conflict-
   free merge, §5.4): two hosts that genuinely race the first advertisement** — each
   legitimately minting an anchor fact over a different antichain, with no single first
   advertiser to defer to and no superseding order between the two facts — converge by
   merging the conflicting anchor facts on the **same** `min`-`ChangeId` key: every
   replica holding both deterministically keeps the byte-order-least anchor and treats
   the other as superseded, so all hosts agree **without a lease or single-writer
   lock**. A conformance test must assert that two divergent concurrent anchor facts,
   once both propagate, resolve to the **same** `<thread>` tip on every replica.
   **Transmission
   must not leak:** no withheld-state count, gap, or placeholder; the
   `ObjectType::Visibility` record is itself gated so it is served only when its
   state is **to a client** (§8.4 client-facing gate).
   **Cross-path disclosure ordering** is forward-only under every interleaving, and
   a served state is **never re-embargoed** (one-way tier constraint, §5.4) — reject
   a `StateVisibility` that would lower an already-served state's tier. The serve-time
   tier decision **reads the persisted `StateVisibility` record** (the latest
   non-superseded record, `redaction.rs:174`-style), **never recomputed** from
   wall-clock or per-host config (§5.4 monotonic-fact rule); this host is the single
   authority that fires scheduled `embargo_until` promotions, materializing a
   persisted promotion record before the first public serve. **Promotion-record
   propagation/coordination (propagate-before-gate, §5.4):** the materialized
   promotion record replicates host-to-host as an authoritative fact — over the
   record-sync path redactions already use (`crates/client/src/grpc_hosted/sync.rs:268-302`),
   **not** behind the §8.4 client gate — and a secondary host **confirms it holds
   the authoritative records before it gates `S`**; a host that cannot confirm fails
   toward last-known-public (withhold serving or serve last-known-public, **never
   re-hide**), so no lagging or clock-skewed host re-embargoes a state already
   served elsewhere (O5). Define the
   grant-role → `AudienceTier` mapping (resolves O2); optional `PromoteVisibility`
   RPC. Blocked by #1; `Scope: multi` (heddle proto + weft).
5. **impl(bridge): embargo DAG integrity across ALL Git ref-publishing surfaces +
   scheduled promotion.** Forward-only disclosure for the Git mirror, applied to
   **every** Git-side surface — not just the branch ref. Route all of them through
   one shared `resolve_frontier(audience, raw_target) -> served_target` chokepoint
   (the close-the-class structural fix, §5.3) and add a conformance test asserting
   no surface wires a raw `get_thread`/`get_marker`/`mapping`-iterated OID into a
   publish call. The surfaces this issue MUST cover:
   - **Branch ref-sync** (`git_export.rs:277-288`): lag `refs/heads/main` to the
     frontier (last all-public ancestor), not the raw `get_thread` tip (`:278`);
     FF-only (`ensure_commit_update_fast_forward`, `git_core.rs:2446`).
   - **Marker→tag sync** (`git_export.rs:290-296`, `sync_marker_to_tag`,
     `git_sync.rs:157`): **withhold `refs/tags/<marker>` entirely until the marked
     state is served** — a tag names a specific state and cannot lag; it is
     conflict-on-mismatch, not FF (`git_sync.rs:163-170`), so it cannot be silently
     corrected once published at a hidden OID (this is the cid 3325554161 item —
     explicitly in the work list, not only in the spike body).
   - **State notes `refs/notes/heddle`** (writes at `git_export.rs:242-245,252-261`
     and `git_core.rs:1105-1109`; forced mirror `+refs/notes/*`, `git_core.rs:300`):
     write/publish a `HeddleNote` only for a **served** state — the note carries
     `change_id`/attribution/agent/signals (`git_notes.rs:33-56`), the Git-mirror
     analogue of the State-header leak; the whole-mapping backfill loop must filter
     by the served set, never iterate the raw mapping.
   - **Notes-ref rebuild on withhold (history-bearing surface).** Filtering the
     current write/tree is **not** sufficient here: the notes ref is history-bearing
     — `write_note` parents each notes commit on the prior head (`read_notes_head`
     `git_notes.rs:180`; `new_commit_as(..., parents=[prev_head])`
     `git_notes.rs:195-204`), so an embargoed-state note written in a *past* notes
     commit stays reachable through the ref's parent chain and the forced
     `+refs/notes/*` mirror ships the whole chain. This issue MUST **rebuild the
     published notes ref on withhold** — reconstruct (squash/rewrite) its commit
     chain so **no embargoed-state note object is reachable through the published
     history**, not merely filter the tip tree — forward-only on the *observable note
     content* (served-state notes never drop/rewrite; one-way tiers § 5.4 make the
     published set grow-only), §5.3 history-bearing-ref rule. `resolve_frontier`
     returns a *rebuilt ref* for history-bearing refs (vs a lagged/withheld tip for
     single-target refs); the conformance test must additionally assert the
     published notes ref's **reachable history** contains no note object for an
     unserved state — not just that the current tip tree omits it.
   - **`HEAD` symref + bulk export/push** (`git_core.rs:2398-2407,751,678-689`):
     `HEAD` resolves to a served branch (never a wholly-embargoed one); the bulk
     copy inherits soundness from the per-ref gating above.
   - **Multi-root merge frontier** (§5.3): when the antichain has ≥2 incomparable
     maximal served states, publish each antichain member **other than the one
     `<thread>` names** (every sibling line) under a deterministic
     synthetic `refs/heads/<thread>@<full-changeid>` (append-only, retires once a
     served descendant reunifies the fork), so a plain `git clone` fetches every
     visible side. **Invariant C (collision-proof naming, §5.4): the synthetic name
     carries the full `ChangeId` via `ChangeId::to_string_full()` (`hash.rs:129`),
     never a truncatable `short()`/prefix form (`:137`/`:167-170`)**, so two siblings
     that share any prefix still map to distinct refs and neither frontier root is
     overwritten or made undiscoverable; a conformance test must assert prefix-sharing
     siblings get distinct, individually-fetchable refs. `<thread>` itself only ever
     advances FF **along its own line** to the maximal served descendant of its prior
     tip, else retains its prior tip — **never moves sideways to a sibling member and
     never regresses** (antichain ref-placement stability, §5.3; structurally enforced
     by `ensure_commit_update_fast_forward`, `git_core.rs:2446`). A conformance test
     must assert `refs/heads/<thread>` is forward-only across re-exports: it never
     names an incomparable antichain sibling and never regresses.
   Audience-aware minting skips under-tier states, but the **ref/tag/note tips are
   decided by the frontier, never the raw mapped state**, so embargoed commits *and
   their descendants* are absent. Disclosure FF-appends the real commits, each
   minted once (`export_state` mapping-skip, `git_export.rs:218-223`); never re-mint
   or force-push a published commit (FF guard forbids it). **No stub commit** —
   stub-swap and parent-reparent are ruled out (§5.0.1, resolves O3). Cross-path
   disclosure ordering is forward-only under every interleaving (§5.3); a served
   commit is never re-embargoed (§5.4). **Scheduled `embargo_until` promotion is
   materialized as a persisted monotonic fact:** the authoritative host appends a
   superseding `public` `StateVisibility` + `OpRecord::StateVisibilityPromote`
   *before* the first public serve, and visibility is read from that record, never
   recomputed from wall-clock; the materialized promotion **propagates to every
   other serve/export host and is confirmed present there *before* that host gates
   the state** (propagate-before-gate, §5.4) — and a host missing the authoritative
   records **fails toward last-known-public, never re-hides** — so no lagging or
   clock-skewed host can re-embargo a state already served elsewhere (resolves O5,
   §5.4). Blocked by #2.
6. **decision/spike: unify `AnnotationVisibility` into a shared `VisibilityTier`**
   across annotations/discussions/states (resolves O4). Small; can fold into #1
   if the maintainer approves the unification up front.
