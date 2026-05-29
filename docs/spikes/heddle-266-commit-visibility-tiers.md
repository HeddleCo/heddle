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
  auto-promotion introduces a wall-clock trust question (§9, O5).

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
   (`crates/cli/src/bridge/git_sync.rs:134`, `git_core.rs:2271-2291`), which
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
`RedactionsBlob::latest`, `redaction.rs:174`), with `embargo_until` evaluated
against wall-clock at serve time.

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
everything else is absent. The two surfaces are the same gate projected:

- **Wire serve (weft, authoritative).** `ListRefs` omits any thread whose tip is
  not in the caller's served set. `Pull`'s planner walks from the tip and
  **halts at the first under-tier state**: it emits neither that state's
  `ObjectType::State` object — so no `intent`/`attribution`/`signature` header
  ever leaves the host, closing cid 3324616942 — nor anything reachable only
  through it. Because the walk stops at the embargo boundary, the under-tier
  state's **descendants are never reached either**: the downward-closure is
  enforced structurally, not by a second filter.
- **Git-bridge export (heddle OSS).** `export_state` (`git_export.rs:28`) already
  loads the `State` (hence its `ChangeId`) but currently passes only
  `&state.tree` to `export_tree` (`git_export.rs:41`) and takes no audience (its
  `--audience` is planned — inline note at `git_export.rs:44-47`). Add an
  `AudienceTier` parameter and **halt the public-audience export walk at the
  first under-tier state**. `refs/heads/main` fast-forwards only through the last
  all-public ancestor; the embargoed commit and its descendants are simply not
  exported. This is the Git-mirror projection of the very same gate — the public
  mirror only ever holds the visible, ancestry-closed closure.

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
named dissolves; what remains is an ancestry-closed visibility walk that halts at
the embargo boundary. (r3's conclusion stands — do **not** reuse
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
- **Scheduled:** an `embargo_until` timestamp; the serve filter treats a state as
  `Public` once wall-clock passes it, without needing a write. (The trust model
  for clock-based auto-reveal is open question O5.)

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
| bridge | add `AudienceTier` param to `export_state` (`:28`, holds the `ChangeId`); for the public mirror **halt the export walk** at the first under-tier state so the embargoed commit *and its descendants* are absent (forward-only; tip lags to the last all-public ancestor, §7.1 step 3) — disclosure FF-appends, never re-mints/force-pushes (`ensure_commit_update_fast_forward`, `git_core.rs:2271`; mapping-skip `git_export.rs:218-223`); **no stub commit** (stub-swap/parent-reparent ruled out, §5.0.1) — not in `export_tree` (`:97`, tree-keyed, no audience) | per-state mint (`git_export.rs:84-93`), FF guard (`git_core.rs:2271`) |
| `proto` / wire | new `ObjectType::Visibility` in the sync plan (mirroring `emit_redaction_plan`), itself gated — a record is served only when its state is served, so it never leaks an embargoed `ChangeId`/tier/date (§8.4); **new** tier-aware **downward-closed reachability gate** — serve the forward closure of the ancestry-closed visible set, halting the walk at the first under-tier state (no `ObjectType::State` header for an embargoed commit). NOT a `collect_excluded` extension (root-exclusion over-withholds, §5.3) and no set difference is needed (a child of a hidden parent is never served, so nothing shared to subtract) | `emit_redaction_plan` (`object_graph.rs:346`); contrast `collect_excluded` (`:360`) |
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
   A reader with public tier calls `Pull`. The weft serve planner walks from the
   thread tip and **halts at `N`** (the first under-tier state): it emits neither
   `N`'s `ObjectType::State` object — so none of `N`'s `intent` / `attribution` /
   `signature` / timestamp metadata travels, because there is no header-only form
   to serve (§5.0, closing cid 3324616942) — nor anything reachable only through
   `N`. Because `N+1` has `N` as an ancestor, `N+1` is **also** absent
   (downward-closure, §5.0/§5.3); the public `ListRefs` shows the thread tip
   lagged to the last all-public ancestor `N-1`. The public clone learns nothing
   of `N` or `N+1` — not their content, not their metadata, only that the public
   tip is `N-1`.

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
   Git-projection of the step-2 gate: the bridge's public-audience export walk
   halts at `N`, so `N` and its descendants `N+1…` are not exported and
   `refs/heads/main` stays at the last all-public ancestor `N-1`. There is no way
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
5. **Disclosure — forward-only, never a swap.** On 2026-07-01 the `embargo_until`
   lapses (or someone runs `heddle visibility promote N`, appending a superseding
   `public` `StateVisibility` record + `OpRecord::StateVisibilityPromote`, §5.4).
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
     `crates/cli/src/bridge/git_core.rs:2271-2291`) — there is no force path in this
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

1. **Open for review.** `heddle visibility set @ --tier reviewers:secteam` on the
   tip (`reviewers:secteam` → `Restricted { scope_label: "secteam" }`, which —
   unlike `private:` — is also visible to the internal trusted set so CI/tooling
   can read the proposed commit, §5.2). A `StateVisibility` record lands on the
   tip state.
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
  `git_core.rs:2271`) and a commit's OID is fixed by its tree + parents
  (`export_state`, `git_export.rs:84-93`), so a published commit's identity can
  never change. **Resolution:** the public-audience export walk halts at the
  first under-tier state, so the embargoed commit **and its descendants** are not
  published; `refs/heads/main` lags to the last all-public ancestor, and
  disclosure FF-appends the real commits with true OIDs (§5.0/§7.1 step 3/step 5).
  This is the Git-mirror projection of the one downward-closed gate (§5.3) — no
  separate strategy. The earlier "permanent stub commit" fallback is **dropped**:
  it would publish the descendant against a synthetic stub parent (a partial
  embargoed-commit view + a parent-edge rewrite), which §5.0.1 rules out. Stub-swap
  and graft-reparent `N+1` onto `N-1` are likewise rejected (both change a
  published OID → non-FF rewrite; break change-id stability + signatures). No
  remaining sub-question — there is one strategy, not an A-vs-B choice.
- **O4 — enum unification + new tier.** Promote `AnnotationVisibility` → a shared
  `VisibilityTier` across annotations/discussions/states (recommended; already
  reused by two consumers) and add the strictest `Private { scope_label }` variant
  + its `visible()` arm above the all-seeing-`Internal` arm (§5.2), vs a separate
  `StateAudience` (more isolation, more duplication of the `visible()` table). The
  `Private` arm changes `visible()` for *all* consumers (annotations/discussions
  could use it too) — confirm that's acceptable, or scope the arm to states.
- **O5 — clock trust for `embargo_until`.** Auto-promotion on wall-clock means a
  client/server with a skewed or rolled-back clock could reveal early or hold
  late. Whose clock is authoritative — only the weft serve host? Should
  auto-promotion be advisory (the serve host still re-checks) rather than
  client-evaluated?
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

1. **impl(objects/repo): `StateVisibility` object + per-state sidecar store.**
   Add `StateVisibility` / `StateVisibilityBlob` (objects), the `visibility/`
   sidecar dir + read/write + `has_visibility_for_state` (repo/store), modeled on
   `Redaction`/`RedactionsBlob`. Blocked by this spike.
2. **impl(repo/bridge): audience plumbing + operator-local courtesy stub.** Extend
   `visible()` with the strictest `Private` arm (above the all-seeing-`Internal`
   arm); thread an `AudienceTier` through the checkout entry (above
   `materialize_tree`) and through `export_state` (which already holds the
   `ChangeId`). The bridge enforces the public mirror by **halting the export walk**
   at the first under-tier state (issue #5), emitting absence. The render-stub is
   only the **operator-local checkout courtesy** at the **state-walk level** — *not*
   the blob-keyed `materialize_blob`/`export_tree` (no `ChangeId`/audience), and
   *not* a public-mirror surface (§5.0/§5.3). Blocked by #1.
3. **impl(oplog/cli): tier records + `heddle visibility` verb family.**
   Tail-append `StateVisibilitySet`/`StateVisibilityPromote`; implement `set` /
   `promote` / `show` / `list`; wire the config-default resolution chain. Blocked
   by #1.
4. **impl(weft, cross-repo): authoritative serve-side downward-closed gate.** Gate
   `ListRefs`/`Pull` by caller tier via the **tier-aware downward-closed
   reachability pass** (§5.3): serve the forward closure of the ancestry-closed
   visible set, halting the walk at the first under-tier state so no
   `ObjectType::State` header for an embargoed commit (or any descendant) is
   emitted. *Not* an extension of the root-exclusion `collect_excluded` (which
   over-withholds blobs a visible child shares with a hidden parent), and no set
   difference is needed — a child of a hidden parent is never served, so there is
   nothing shared to subtract. Define the grant-role → `AudienceTier` mapping
   (resolves O2); optional `PromoteVisibility` RPC. Blocked by #1; `Scope: multi`
   (heddle proto + weft).
5. **impl(bridge): embargo DAG integrity + scheduled promotion.** Forward-only
   disclosure for the Git mirror: halt the public-audience export walk at the first
   under-tier state so it **and its descendants** are not published, so
   `refs/heads/main` lags to the last all-public ancestor; disclosure FF-appends
   the real commits, each minted once (`export_state` mapping-skip,
   `git_export.rs:218-223`). Never re-mint or force-push a published commit — the
   FF guard (`ensure_commit_update_fast_forward`, `git_core.rs:2271`) forbids it.
   **No stub commit** — stub-swap and parent-reparent are ruled out (§5.0.1,
   resolves O3). `embargo_until` auto-promotion at serve (resolves O5). Blocked by
   #2.
6. **decision/spike: unify `AnnotationVisibility` into a shared `VisibilityTier`**
   across annotations/discussions/states (resolves O4). Small; can fold into #1
   if the maintainer approves the unification up front.
