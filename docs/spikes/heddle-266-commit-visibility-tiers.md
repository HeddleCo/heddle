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
   publish together or both stay private. Needed: `N+1` lands publicly while `N`
   stays embargoed until a disclosure date.

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
The recommended design (§5) therefore treats the **serve-side withhold as the
source of truth** and reuses the redaction render-stub as a second, in-depth
layer for the local-checkout and Git-bridge chokepoints.

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
  And the field-in-the-object gives no way to serve a state's *header* (so the
  DAG stays walkable) while withholding its *content*: the field is inside the
  very object you're trying to withhold. Rejected.

### Design B — state-keyed sidecar (mirror `Redaction`) + serve filter — **recommended**

A `StateVisibility` record keyed by `ChangeId`, stored in a per-state sidecar
`StateVisibilityBlob`, enforced (a) hard at the wire serve layer and (b) soft as
a render-stub at the materialize + git-export chokepoints. Promotion is an
additive superseding record (or an `embargo_until` lapse), never a state
mutation. Detailed in §5–§8.

- **Pros:** preserves `State` immutability and signatures (the sidecar is
  outside the hashed bytes, exactly like `Redaction`, `review_signatures`,
  `discussions`); promotion is additive and audit-friendly; reuses the entire
  proven substrate — `AudienceTier` + `visible()` filter + namespace-default
  resolution + the signed / supersede / fail-closed-trust patterns; enforces at
  the same chokepoints redaction already guards, so the integration points are
  known; the serve-side filter is a genuine hard boundary.
- **Cons:** a two-layer mental model (state + visibility sidecar); the *hard*
  serve filter must be implemented in the closed weft serve path (this OSS spike
  can specify the object, the records, the wire-plan exclusion, and the
  cooperative render, but the authoritative withhold lands in weft); time-based
  auto-promotion introduces a wall-clock trust question (§9, O5).

### Design C — tier-per-ref (separate threads)

Model each audience as its own thread/ref — `main` (public),
`main@review` (reviewer-scoped) — and gate each ref with the existing
repo/namespace grant model.

- **Pros:** no new per-commit object; reuses thread-level grants; the serve
  filter degenerates to "can you see this ref."
- **Cons (disqualifying for the core case):** this is exactly the status quo the
  issue calls out as the gap — "today this requires splitting into a separate
  private thread." It cannot express the embargoed-fix case, where `N` (hidden)
  and `N+1` (visible) sit on the *same line of history*: they would have to live
  on divergent refs that then need reconciliation. Rejected as the primary
  design; retained as the fallback that already works for coarse cases.

---

## 5. Recommended design (Design B), in detail

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

### 5.2 Reuse the tier enum — do not invent a parallel one

The three audiences the issue names map directly onto the *existing* tier
vocabulary, which is already shared across annotations and discussions:

| Issue audience  | Tier value (reuse `AnnotationVisibility`/`AudienceTier` shape)        |
|-----------------|----------------------------------------------------------------------|
| private         | `Internal` (and the namespace fallback) — most restrictive           |
| reviewer-scoped | `Restricted { scope_label }` or `TeamScoped { team_id }` — a named reviewer audience |
| public          | `Public` — universally visible                                       |

The recommendation is to **promote `AnnotationVisibility` to a shared
`VisibilityTier`** used by annotations, discussions, *and* states, rather than
forking a third enum (it is already reused by two consumers; a third is natural).
That unification is itself a small decision (§9, O4) — if the maintainer prefers
isolation, a `StateAudience` enum with the same four variants is the fallback,
at the cost of duplicating the `visible()` table.

The **reader** side reuses `AudienceTier` and its existing string grammar
(`internal | public | team:NAME | restricted:LABEL`, `visibility.rs:60`) so
"clone/fetch/export as audience X" needs no new vocabulary.

### 5.3 Enforcement — hard at serve, soft in depth

- **Hard (authoritative), wire serve — weft.** `ListRefs` omits any thread whose
  tip is above the caller's tier; `Pull`'s object planner excludes states (and
  objects reachable *only* through them) above the caller's tier — extend the
  existing `collect_excluded` pass (`object_graph.rs:360`) with a tier predicate
  keyed off the visibility sidecar and the caller's `AudienceTier` (derived from
  the auth context + per-thread grant, §2.6). Under-tier bytes never leave the
  host.
- **Soft (defense in depth), local + bridge — heddle OSS.** At `materialize_blob`
  (`repository_materialization.rs:591`) and `export_tree`
  (`git_export.rs:127`), an under-tier state renders a **visibility stub** (a
  short notice naming the tier and the promotion date, like the redaction stub at
  `redaction.rs:106`) instead of its content. This protects a self-hosted Git
  mirror and a local checkout shared across audiences even where the hard wire
  filter doesn't apply.

The two layers compose exactly as redaction's two chokepoints do; the wire filter
is the boundary you can rely on against an adversarial puller, the stub is the
cooperative belt-and-suspenders.

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
| `objects` crate | new `StateVisibility` + `StateVisibilityBlob` objects; (preferred) rename `AnnotationVisibility` → shared `VisibilityTier` | mirrors `Redaction`/`RedactionsBlob` (`redaction.rs:29,133`) |
| object store | per-state `visibility/` sidecar dir + read/write + `has_visibility_for_state` | mirrors redactions dir (`fs_paths.rs:46-50`, `repository_redaction.rs:490`) |
| `oplog` | tail-append `StateVisibilitySet` / `StateVisibilityPromote` | tail-append rule (`oplog_types.rs:14-21`) |
| `repo` resolve | visibility stub at `materialize_blob` and a state-tier predicate; reuse `visible()` | redaction stub (`repository_materialization.rs:591`), filter (`visibility.rs:148`) |
| bridge | visibility stub at `export_tree`; **stub-commit** for embargoed states (§7.1) | redaction stub (`git_export.rs:127`), per-state mint (`git_export.rs:213-246`) |
| `proto` / wire | new `ObjectType::Visibility` in the sync plan (propagate soft records); tier predicate in `collect_excluded` | `emit_redaction_plan` / `collect_excluded` (`object_graph.rs:346,360`) |
| weft (closed) | **authoritative** server-side tier filter in `ListRefs`/`Pull`; grant-role → `AudienceTier` mapping; optional `PromoteVisibility` RPC + scheduler | `RepoSyncService` (`service.proto:8`); role substrate (`contribution-grant-flows.md` §1) |
| config | `[namespace.<name>] default_state_visibility` + repo-wide default; reuse the resolution chain | `resolve_default_visibility` (`namespace_policy.rs:68`) |

No change to `State` itself — its tail-append invariant and signatures are
preserved precisely because visibility lives in the sidecar.

---

## 7. Worked examples

### 7.1 Embargoed security fix

Setup: public thread `main`, mirrored to `refs/heads/main`. The thread's default
tier is `public` (resolved from config, §8). Commit `N` describes the exploit;
commit `N+1` is the fix. `N+1.parents = [N]` (by `ChangeId`, `state_core.rs:207`).

1. **Declare the embargo.** `heddle visibility set N --tier private --until
   2026-07-01T00:00:00Z --sign-with ops.pem`. This writes a `StateVisibility`
   record (`tier = Internal`, `embargo_until = 2026-07-01`) into `N`'s sidecar
   and an `OpRecord::StateVisibilitySet` (§5.5). `N+1` keeps the thread default
   (`public`).
2. **Public pull (hard).** A reader with public tier calls `Pull`. The weft
   serve planner excludes `N`'s tree and the blobs reachable *only* through `N`
   (extended `collect_excluded`, §5.3); `N`'s state *header* may still travel so
   the DAG is walkable, but its content does not. `N+1` serves in full. The
   public clone can check out the fix; it cannot reconstruct the exploit.
3. **Public Git mirror (DAG integrity).** The bridge must export `N+1` to
   `refs/heads/main`, and `N+1`'s Git parent must resolve. The recommended answer
   is a **stub commit**: `export_state` mints a Git commit for `N` whose tree is
   the visibility stub (and whose message says "embargoed until 2026-07-01") so
   the commit chain stays intact, while no exploit content leaks. (The
   alternative — re-parenting `N+1` onto `N-1` — is a history rewrite that breaks
   change-id stability and is rejected; see O3.)
4. **Reviewer/maintainer pull.** A caller whose effective tier ≥ private (via
   grant) pulls and sees `N` in full — same objects, no stub.
5. **Disclosure.** On 2026-07-01 the `embargo_until` lapses (or someone runs
   `heddle visibility promote N`, appending a superseding `public` record +
   `OpRecord::StateVisibilityPromote`). The next public `Pull` / bridge export
   serves `N`'s real content; the stub commit is replaced by the real tree on the
   next export.

### 7.2 In-flight PR with a reviewer audience

Setup: a feature thread `feat/x`. Its default tier is `private` (nothing public
until merge).

1. **Open for review.** `heddle visibility set @ --tier reviewers:secteam` on the
   tip (`reviewers:secteam` → `Restricted { scope_label: "secteam" }`). A
   `StateVisibility` record lands on the tip state.
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
**inherit**, resolved through the *existing* chain (`namespace_policy.rs:68`):

1. explicit `heddle visibility set` on the state (the deliberate exception),
2. the thread's default tier,
3. `[namespace.<name>] default_state_visibility` in config,
4. repo-wide default,
5. hard-coded fallback — **`private`** (the safe "don't know who should see this"
   choice, mirroring the `Internal` fallback at `namespace_policy.rs:11`).

So a normal public thread sets its default once; every commit is public without a
flag. The embargoed-fix case is the *only* time you reach for `visibility set`,
to mark the one exceptional commit. This is the whole ergonomic argument: the
common path has **zero** new flags; the rare path has one verb.

### 8.2 The verb family (minimal, mirrors `redact`)

```
heddle visibility set <state> --tier <public|reviewers:LABEL|private> [--until RFC3339]
                              [--all-states] [--sign-with PEM] [--sign-algo A]
heddle visibility promote <state>            # supersede with a more-open tier now (audited)
heddle visibility show <state>               # the effective tier + record chain
heddle visibility list                       # every non-default tier in the repo
```

Justification, verb by verb (none is "for completeness"):

- **`set`** — the irreducible declaration. Someone must mark the exception; it
  cannot be defaulted away. `--tier` is a single enum value (not one flag per
  tier). `--until` folds the scheduled-promotion case into `set` instead of a
  separate "schedule" verb. `--all-states`, `--sign-with`, `--sign-algo` are
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
rather than minting a parallel selector. No new reader-side vocabulary.

### 8.4 Wire-trust reuse

Soft visibility records that propagate over the wire are governed by the same
**fail-closed trust list** model as signed redactions (`RedactTrustCommands`,
`commands_redact.rs:41`): a peer's signed visibility record is honored only if
its key is trusted, so a malicious peer cannot *forge* a more-open tier. (The
*hard* boundary does not depend on this — it's the server withholding bytes; the
trust list governs the cooperative records that ride alongside.)

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
- **O3 — bridge DAG strategy.** Stub-commit (recommended: mint `N` with a stubbed
  tree so `N+1`'s parent resolves) vs graft-reparent `N+1` onto `N-1` (rejected:
  rewrites history, breaks change-id stability + signatures). Confirm
  stub-commit; specify the exact stub-tree shape.
- **O4 — enum unification.** Promote `AnnotationVisibility` → a shared
  `VisibilityTier` across annotations/discussions/states (recommended; already
  reused by two consumers), vs a separate `StateAudience` (more isolation, more
  duplication of the `visible()` table).
- **O5 — clock trust for `embargo_until`.** Auto-promotion on wall-clock means a
  client/server with a skewed or rolled-back clock could reveal early or hold
  late. Whose clock is authoritative — only the weft serve host? Should
  auto-promotion be advisory (the serve host still re-checks) rather than
  client-evaluated?
- **O6 — signature scope.** A `State.signature` signs the state bytes; the
  visibility sidecar is outside that, so an embargo declaration needs its **own**
  signed payload (mirror `Redaction::canonical_signing_payload`,
  `redaction.rs:67`). Confirm the canonical payload fields for `StateVisibility`.
- **O7 — header-visible vs fully-withheld.** Should an under-tier state's
  *header* (id + parents, so the DAG is walkable) travel while its content is
  withheld, or should the whole state be withheld and the child served with a
  synthetic parent pointer? §7.1 assumes header-visible (simpler, DAG stays
  intact); confirm this doesn't itself leak sensitive metadata (commit message,
  author, timestamp) — those may need stubbing too.

---

## 10. Proposed follow-up implementation issues (NOT filed — for maintainer triage)

Per spike discipline, these are proposed only; the orchestrator confirms scope
before filing.

1. **impl(objects/repo): `StateVisibility` object + per-state sidecar store.**
   Add `StateVisibility` / `StateVisibilityBlob` (objects), the `visibility/`
   sidecar dir + read/write + `has_visibility_for_state` (repo/store), modeled on
   `Redaction`/`RedactionsBlob`. Blocked by this spike.
2. **impl(repo/bridge): resolve-layer visibility stub (cooperative tier).** Stub
   under-tier states at `materialize_blob` and `export_tree`; reuse the
   `visible()` filter for the state-tier predicate. Blocked by #1.
3. **impl(oplog/cli): tier records + `heddle visibility` verb family.**
   Tail-append `StateVisibilitySet`/`StateVisibilityPromote`; implement `set` /
   `promote` / `show` / `list`; wire the config-default resolution chain. Blocked
   by #1.
4. **impl(weft, cross-repo): authoritative serve-side tier filter.** Filter
   `ListRefs`/`Pull` by caller tier (extend `collect_excluded`); define the
   grant-role → `AudienceTier` mapping (resolves O2); optional `PromoteVisibility`
   RPC. Blocked by #1; `Scope: multi` (heddle proto + weft).
5. **impl(bridge): embargo DAG integrity + scheduled promotion.** Stub-commit for
   embargoed states so child Git parents resolve (resolves O3); `embargo_until`
   auto-promotion at serve (resolves O5). Blocked by #2.
6. **decision/spike: unify `AnnotationVisibility` into a shared `VisibilityTier`**
   across annotations/discussions/states (resolves O4). Small; can fold into #1
   if the maintainer approves the unification up front.
