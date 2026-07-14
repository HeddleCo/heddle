# Contribution grant flows — design spike

> **Status:** spike (doc-only). Replaces the closed PR #241 / r1 design,
> which framed external contributors as anonymous signers. That premise
> was wrong: anonymous (anon-biscuit) callers can only read public
> state. All writes — sign, comment, discussion turns — require an
> authorized hosted-account identity plus a role grant on the target
> repository (direct, namespace-inherited, or invited).
>
> The spike's job is therefore **not** to design an anon-write path.
> It is to design the **UX layer + missing RPCs** that sit on top of
> the existing role substrate so the three grant flows
> (maintainer-initiated invite, user-initiated request, namespace
> inheritance) are surfaced coherently. This unblocks heddle#27
> (CONTRIBUTING.md) so the contribution path can be documented
> accurately.
>
> **Contract note:** RPC, package, and `crates/grpc` references below describe
> the pre-cutover implementation inspected by this spike. New shared API work
> belongs in `HeddleCo/api` under `heddle.api.v1alpha1`; ADR 0048 and its
> migration manifest supersede the old package guidance.
>
> All paths under `crates/weft-server/...` and `crates/grpc/...` in
> this document refer to the sibling **weft** and **heddle** repos
> respectively. This file lives in heddle because the consumer
> (CONTRIBUTING.md) does.

---

## §1 Current substrate

The role + capability substrate already lives in
`crates/weft-server/src/access/` (in the weft repo). The pieces:

### Roles

`PgHostedRole` is an ordinal-comparable enum (asserted in
`access/enforce.rs:159-166`):

```
Reader < Developer < Maintainer < Admin < Owner
```

`require_role_on_*` succeeds iff the caller's effective role on the
target resource is `>= required` (`access/enforce.rs:7-8`,
`enforce.rs:80-106`, `enforce.rs:112-139`).

### Capabilities

The `Capability` enum (`access/scope.rs:34-48`) enumerates 12
canonical capabilities:

```
RepoRead, RepoWrite, RepoAdmin,
ThreadRead, ThreadWrite,
AgentSpawn, AgentRead, AgentUpdate,
GrantRead, GrantWrite,
NamespaceAdmin,
PresenceRead
```

These are the wire tokens parsed out of biscuit scope strings
(`access/scope.rs:206-254`). For the grant-flow UX the relevant
capabilities are `GrantRead` and `GrantWrite`, which already exist —
no new capability tokens are needed.

### Resource hierarchy + inheritance

`access/resource.rs` is the canonical parent-walker:

- `ResourceKind` covers `Namespace`, `Repo`, `Thread`, `Context`
  (`resource.rs:32-38`).
- `resolve_parent` walks one step up:
  `Repo("org/acme/heddle") → Namespace("org/acme")`;
  `Namespace("org/acme") → Namespace("org")` (`resource.rs:109-161`).
- `require_role_on_repository` calls
  `effective_role_for_repository`, which walks the namespace ancestors
  and returns the **highest** matching grant —
  direct repo grant OR any ancestor-namespace grant
  (`enforce.rs:108-139`, contract documented inline).

### Scope strings

Token forms accepted by `parse_scope` (`access/scope.rs:149-165`,
`scope.rs:206-254`):

- `ns:{namespace_path}` — namespace binding.
- `repo:{namespace_path}/{repo_slug}` — repo binding; resolves to its
  parent namespace at enforcement time.
- `repo:*` — global wildcard (admin-only at issuance,
  `scope.rs:340-342`).
- Bare capability tokens (`repo:write`, `grant:write`, …).
- `staff`, `staff:*`, `*` — operator short-circuit
  (`scope.rs:128-147`).

`covers_namespace` resolves ancestor coverage so a grant on `org/acme`
implies coverage of `org/acme/foo/bar` (`scope.rs:191-204`).

### Scope-escalation guard

`check_no_escalation` (`scope.rs:332-348`) blocks a non-staff caller
from issuing scopes that include a staff marker or `repo:*` wildcard,
and rejects target scopes with no namespace binding. This is the
substrate the grant-flow handlers must lean on — a maintainer
inviting another contributor must not be able to grant a role above
their own; the syntactic guard plus the per-namespace role lookup in
the handler enforces that.

### Existing grant-management RPCs

The hosted gRPC surface (`crates/grpc/proto/heddle/v1/service.proto`,
in the heddle repo) already exposes:

| RPC | proto:line | Body shape |
|---|---|---|
| `CreateGrant` | `service.proto:25` | `CreateGrantRequest{subject, role, GrantTargetRef target, client_operation_id}` (proto:507-513) |
| `ListGrants` | `service.proto:26` | `ListGrantsRequest{resource}` (proto:493-495) |
| `UpdateGrant` | `service.proto:27` | `UpdateGrantRequest{subject, role, target, client_operation_id}` (proto:514-520) |
| `DeleteGrant` | `service.proto:28` | `DeleteGrantRequest{subject, target, client_operation_id}` (proto:521-526) |
| `CreateInvitation` | `service.proto:37` | `CreateInvitationRequest{email, namespace_path, role, expires_at, metadata}` (proto:607-613) |
| `ListInvitations` | `service.proto:38` | — |
| `RevokeInvitation` | `service.proto:39` | — |
| `ListMembers` | `service.proto:40` | — |

`GrantTargetRef` (proto:501-506) is the wire-level "exactly one of
namespace or repo" guarantee — the same shape new RPCs should reuse.

The `user.rs` handler (`server/grpc_hosted_impl/user.rs:489-551`)
implements `CreateGrant`/`ListGrants` and enforces the
`can_manage_namespace` / `can_manage_repository` gate before
returning each row. Idempotency is the existing
`client_operation_id` discipline at tag 15.

**What's already there vs. what the spike adds:** the maintainer
side of the grant graph (invite, create grant, list members) is
already wired. What's missing is (a) the **request-to-contribute**
flow — there is no RPC for a user-without-a-grant to ask for one —
and (b) the **tapestry UX** that surfaces the three flows
coherently.

---

## §2 Anon RPC surface (read-only-public)

`Subject::Anon(uuid)` callers are produced by `mint_anon`
(`biscuit.rs:455-499` — minted from the `MintAnonBiscuit` RPC) and
carry **no rights, no PoP, no device binding** — identity only.
The verifier stamps the subject; each handler then decides whether to
admit anon callers.

The contract per `mint_anon`'s rustdoc
(`biscuit.rs:459-475`):

> Anon callers are admitted to anon-allowed RPCs and rejected from
> user-only ones (`Status::failed_precondition` per spike §3.4 / §5.2).

The user-only gate is centralised in `require_user_subject`
(`server/grpc_hosted_impl/auth_helpers.rs:186-193`):

```rust
pub fn require_user_subject(verified: &VerifiedBiscuit) -> Result<Uuid, Status> {
    match verified.subject {
        Subject::User(uuid) => Ok(uuid),
        Subject::Anon(_) => Err(Status::failed_precondition(
            "user account required; anon callers cannot create this resource",
        )),
    }
}
```

Anon-admitted RPCs today (audited via grep on `Subject::Anon` in
`server/grpc_hosted_impl/`):

| RPC | location | comment |
|---|---|---|
| `StartReviewAnalysis` | `review.rs:23-83` | anon-allowed; owner-stamped to allow §4.4 promotion migration |
| `GetReviewAnalysisStatus` | `review.rs:85-125` | read-only |
| `GetReviewAnalysisResult` | `review.rs:127-…` | read-only |
| `LinkOAuthIdentity` (anon-promotion path) | `auth.rs:1836` | anon-required by design |

The grant-write surface (`CreateGrant`, `CreateInvitation`,
`DeleteGrant`, etc.) calls `self.require_claims(&request)?` which
falls through to user-subject paths; a follow-up audit should pin a
unit test that anon callers receive `Status::failed_precondition`
from each grant-write handler. The substrate appears correct
(handlers gate on user-subject before any registry mutation) — the
spike does not introduce any anon-admission for the new write RPCs
in §3.

> **Follow-up flag (do not fix in this spike):** the audit above is
> grep-shaped, not a full coverage matrix. The substrate fix to
> add is a single helper in `auth_helpers.rs` along the lines of
> `require_user_subject_for_write` that every grant-mutating
> handler calls, plus a parameterised integration test that loops
> over the write surface and asserts `failed_precondition` for an
> anon caller. File as a sub-impl item under weft (see §5).

---

## §3 Grant-flow UX design

Three flows, all targeting the same `(subject_user_uuid, resource,
role)` substrate triple. The user-facing affordances are different
because the trigger and the gating identity are different.

### §3.1 Maintainer-initiated invite

**Trigger.** A maintainer (anyone with `>= Role::Admin` on the repo
or any ancestor namespace, per `require_role_on_repository`) lands
on the tapestry repo-settings page and clicks "Invite contributor".

**Identity resolution.** The maintainer enters either:

- a tapestry user handle (resolved server-side to a
  `subject_user_uuid` via the existing user-directory lookup), OR
- an email address of someone who may or may not yet have an
  account.

These two cases collapse onto the existing split:

- Handle / known-user → `CreateGrant` directly (the substrate RPC
  exists; idempotent via `client_operation_id`).
- Email / not-yet-a-user → invitation. **The existing
  `CreateInvitation` (`service.proto:37`, body at proto:607-613) is
  namespace-scoped only — it carries `namespace_path`, not a repo
  path.** A maintainer triggering invite from a repo-settings page
  must not have their invite silently widened to the whole parent
  namespace; that is a privilege blunder. This spike therefore extends
  `CreateInvitationRequest` to take a `GrantTargetRef target` oneof in
  place of the bare `namespace_path` field (same shape `CreateGrant`
  uses at proto:501-506), so the invite redeem path mints a grant on
  the same target the maintainer chose:

  ```proto
  message CreateInvitationRequest {
    string email = 1;
    GrantTargetRef target = 2;   // CHANGED: was string namespace_path
    HostedRole role = 3;
    google.protobuf.Timestamp expires_at = 4;
    map<string, string> metadata = 5;
    string client_operation_id = 15;
  }
  ```

  The `Invitation` row gains `target_kind` + `target_path` columns
  (migration listed in §5); the redeem handler reads them and calls
  the same code path `CreateGrant` uses for the matching target kind.
  Justification for extending the existing RPC rather than adding a
  parallel `CreateRepoInvitation`: the wire shape is already
  `(email, target, role, expires_at)` and the redeem path is shared;
  splitting the RPC duplicates both handler and persistence with no
  semantic gain. The field swap is wire-incompatible with
  heddle-grpc 0.7 — this is one of the changes motivating the
  0.7 → 0.8 minor bump in §4 (additive on new RPCs, breaking on
  this one field; consumers are pinned to the workspace so the bump
  is coordinated).

**Picking handle-vs-email.** Recommend offering **both** in the UI
behind a single field that auto-detects (regex on `@`). Justification:
the substrate already supports both, the maintainer's mental model is
"the person I want to add", and forcing them to know whether the
target has signed up yet is needless friction. The handler picks
`CreateGrant` vs `CreateInvitation` based on the resolved-user
lookup result.

**Role-pick guardrail.** The role dropdown is clamped at the
maintainer's own effective role on the resource. The
syntactic guard already lives in `check_no_escalation`
(`scope.rs:332-348`); the handler must additionally consult
`require_role_on_repository` for the caller and refuse to issue a
grant with a `role` above what came back. This is a per-handler
check, not a substrate change.

**Notification.** Out of scope — see §6. v0 surfaces the new grant
on the invitee's next page load (the existing grant-listing path
will pick it up).

**Wire shape.** No new RPCs — `CreateGrant` and `CreateInvitation`
both already exist. The spike's deliverable for §3.1 is the
**tapestry UI** (settings panel with handle/email input, role
dropdown, expiry picker) and the **client-side dispatch logic**
that picks the right RPC.

### §3.2 User-initiated request-to-contribute

**Trigger.** A signed-in user (anon-promoted or signed-up directly)
browses to a public repo or namespace they don't yet have a role on.
The repo settings / contributor surface shows a "Request access"
affordance instead of the editor.

**Trigger probe — new RPC.** `ListGrants` cannot be the probe here:
it's gated on `can_manage_namespace` / `can_manage_repository`
(`user.rs:541-547`), i.e. admin-only. A normal user calling it on a
resource they have no role on would be rejected, which is the
opposite of the signal the UI needs. The spike adds a small
self-introspection RPC any signed-in user can call:

```proto
message GetMyEffectiveRoleRequest {
  GrantTargetRef target = 1;
}
message GetMyEffectiveRoleResponse {
  // Unset when the caller has no effective role on `target`
  // (direct or inherited). Present otherwise; equals the
  // `effective_role_for_repository` walk result.
  optional HostedRole effective_role = 1;
  // When `effective_role` is set, names the resource the grant is
  // actually stored on (direct = `target`, inherited = ancestor ns).
  GrantTargetRef source_resource = 2;
}
```

Gating: `Subject::User` only (anon rejected with
`failed_precondition`). No `can_manage_*` gate — the caller asks
about themselves, no information leak beyond what
`effective_role_for_repository` already enforces (target must be
visible; reject `not_found` on private-and-not-already-visible
resources, same discipline as `enforce.rs:9-12`). The UI hint is
"effective_role is unset → show Request access; effective_role
is set → show the editor".

Alternative considered + rejected: keep the `ListGrants` gate but
admit a self-filtered variant (`ListGrants(resource, only_self=true)`
callable by any signed-in user). Weirder shape: it conflates
"who can manage" with "what is my role", and the natural answer
type is a single role / not a list. The dedicated
`GetMyEffectiveRole` RPC is the clearer primitive.

**Wire shape — new RPC.** No existing RPC covers this flow. New:

```proto
enum RoleGrantRequestStatus {
  ROLE_GRANT_REQUEST_STATUS_UNSPECIFIED = 0;
  ROLE_GRANT_REQUEST_STATUS_PENDING = 1;
  ROLE_GRANT_REQUEST_STATUS_APPROVED = 2;
  ROLE_GRANT_REQUEST_STATUS_DENIED = 3;
  ROLE_GRANT_REQUEST_STATUS_EXPIRED = 4;
}

message RequestRoleGrantRequest {
  GrantTargetRef target = 1;
  HostedRole requested_role = 2;   // MUST be != HOSTED_ROLE_UNSPECIFIED
  string justification = 3;        // optional, max 2000 chars; UI-rendered
  string client_operation_id = 15; // idempotency
}

message RoleGrantRequest {  // the persisted entity
  string request_id = 1;
  string requester_subject = 2;    // subject_user_uuid
  GrantTargetRef target = 3;
  HostedRole requested_role = 4;
  string justification = 5;
  google.protobuf.Timestamp created_at = 6;
  google.protobuf.Timestamp expires_at = 7; // server-set, e.g. +14d
  RoleGrantRequestStatus status = 8;
  google.protobuf.Timestamp responded_at = 9;
  string responder_subject = 10;
  string response_message = 11;
}

message RequestRoleGrantResponse { RoleGrantRequest request = 1; }
```

**Validation.** The handler rejects with
`Status::invalid_argument` if `requested_role ==
HOSTED_ROLE_UNSPECIFIED`. proto3 deserialises omitted enum fields
as the zero variant, so without an explicit check an empty
request would be accepted as an ambiguous-role request and then
either get clamped (silently inventing a role the user didn't ask
for) or written through as zero (a no-permission grant on
approval, depending on substrate). The lifecycle enum is given
its own type rather than a free string so wire drift / typos are
caught at compile time across weft, heddle-grpc, and tapestry —
the same discipline the rest of the proto follows.

**Persistence.** New table `pending_grant_requests`. The natural
"one pending request per (requester, resource)" invariant is
enforced by a **partial unique index** rather than a full
`UNIQUE` constraint:

```sql
CREATE UNIQUE INDEX pending_grant_requests_one_per_target
  ON pending_grant_requests (requester_subject, target_kind, target_path)
  WHERE status = 'pending';
```

A full `UNIQUE(requester_subject, target_kind, target_path)` would
make denied/expired rows permanent blockers — once denied, the
requester could never re-request, which conflicts with the
"refuse a new request within N days of the most recent denied
request" policy below (the policy implies a re-request is
possible after the cool-down). Denied / approved / expired rows
stay in the table for history + the cool-down lookup but do not
participate in the uniqueness check. Status flips to `expired` on
a background sweep after `expires_at`.

**Gating.**

- Requester: must be `Subject::User` (anon callers cannot request
  grants — they have no stable identity to hold the row).
- Target: must exist AND be **publicly listable**. Reject with
  `not_found` on private-and-not-already-visible resources to avoid
  the existence-leak the current `require_role_on_*` discipline
  already takes care to prevent (`enforce.rs:9-12`).
- `requested_role`: clamp to `<= Maintainer` at the substrate. A
  user cannot self-request Admin/Owner; that has to come from an
  existing admin via `CreateGrant`/`CreateInvitation`.

**Anti-spam.** Rate-limit at two layers:

- Per-IP via the existing `PerIpRateLimitLayer`
  (already applied to anon-admitted RPCs; extend the limit class to
  cover this RPC).
- Per-requester-per-resource: the partial-unique pending-only
  index above makes a second concurrent request fail at the DB
  level; on top of that, the handler refuses a new request within
  N days of the *most recent denied* request for the same
  `(requester, target)` pair (N=14 suggested) by querying the
  archived denied rows.

**Approval/denial.** Two new RPCs:

```proto
message ListPendingGrantRequestsRequest {
  GrantTargetRef target = 1;       // scope to a single resource
  bool include_descendants = 2;    // namespace queries can roll up
}
message ListPendingGrantRequestsResponse {
  repeated RoleGrantRequest requests = 1;
}

message RespondToGrantRequestRequest {
  string request_id = 1;
  bool approve = 2;
  HostedRole granted_role = 3;     // when approve=true, MUST be
                                   // != HOSTED_ROLE_UNSPECIFIED; may be
                                   // lower than requested. Ignored when
                                   // approve=false.
  google.protobuf.Timestamp expires_at = 4; // optional grant expiry;
                                   // requires the HostedGrant schema
                                   // change below
  string message = 5;              // optional, surfaced to requester
  string client_operation_id = 15;
}
message RespondToGrantRequestResponse {
  RoleGrantRequest request = 1;    // post-update row
  HostedGrant created_grant = 2;   // only present when approve=true
}
```

**Validation.** When `approve == true`, the handler rejects with
`Status::invalid_argument` if `granted_role ==
HOSTED_ROLE_UNSPECIFIED`. Same proto3-zero-enum reasoning as the
request side: an omitted `granted_role` would otherwise silently
write a zero-role grant on approval, which is either no-permission
(confusing) or substrate-undefined (worse). Explicit reject.

**Approval grant expiry — schema change.** The existing
`HostedGrant` shape (proto:501-…) has no `expires_at` field, and
the `grants` table the substrate writes through (`CreateGrant`
path in `user.rs:489-551`) has no expiry column. The `expires_at`
parameter on `RespondToGrantRequest` therefore requires a
coordinated schema + proto change before it can be honoured:

- Migration: `ALTER TABLE grants ADD COLUMN expires_at TIMESTAMPTZ NULL`
  (NULL = no expiry, preserving today's semantics for existing rows).
- Proto: add `google.protobuf.Timestamp expires_at` to `HostedGrant`
  (additive field, optional / NULL on existing direct-grant rows).
- A background sweep that revokes (deletes / archives) grants past
  `expires_at` — same sweep cadence as the
  `pending_grant_requests` expiry sweep.

These three items are listed in §5 as sub-impl issues alongside the
new RPCs. Until they ship, `RespondToGrantRequest` treats a
non-NULL `expires_at` on the response as a no-op + warning (the
field is accepted on the wire so the proto bump can land
independently, but the substrate ignores it until the migration is
in place). The spike's preference is to ship both together as one
atomic sub-impl batch so the field never has a no-op window.

`RespondToGrantRequest` on `approve=true` runs as a single
transaction: insert the row into the existing `grants` table (same
write path as `CreateGrant`, plus the new `expires_at` column),
mark the request `approved`, return both. On `approve=false`, just
update the request row.

**Gating for the maintainer side.** `ListPendingGrantRequests` and
`RespondToGrantRequest` both require `can_manage_namespace`
(if target is namespace) or `can_manage_repository` (if target is
repo) — the same gates `list_grants` already calls
(`user.rs:541-547`). For `include_descendants`, the registry walks
the namespace subtree and filters to requests the caller can manage,
identical pattern to `list_grants` today.

### §3.3 Namespace-level inheritance UX

The substrate already inherits via `effective_role_for_repository`
(walks ancestor namespaces) and `covers_namespace` (`scope.rs:191-204`).
No new RPC is required — the existing `ListGrants(resource)`
returns *direct* grants on the resource. The UX gap is that the
repo-settings page must also show **inherited** grants, with
attribution to the namespace they were granted on.

**Two options for surfacing inheritance.**

**(A) Extend `ListGrants` with `include_inherited: bool` and a
`source_resource` annotation on each row.** Requires a proto field
add (backwards-compatible — additional fields default to false /
empty) and a registry method that walks the parent chain
(`resolve_parent` in a loop until `None`) collecting grants.
Recommend this option — it keeps the inheritance logic on the
server where the substrate already handles the walk, and the UI gets
a single sorted list with a per-row "inherited from `org/acme/`"
badge.

**(B) Client-side: tapestry calls `ListGrants` on the repo and on
each ancestor namespace, then merges.** Wastes round-trips and
re-implements the walk in TypeScript; reject.

**Wire bump for option A:**

```proto
message ListGrantsRequest {
  string resource = 1;
  bool include_inherited = 2;          // NEW
}
message HostedGrant {
  string subject = 1;
  HostedRole role = 2;
  GrantTargetRef target = 3;
  GrantTargetRef source_resource = 4;  // NEW: where this grant is
                                       // actually stored — equals
                                       // `target` for direct grants;
                                       // an ancestor namespace for
                                       // inherited rows
}
```

Both fields are additive; old clients ignore them and continue to
receive direct-only results. heddle-grpc minor bump suffices.

**Tapestry surface.** Repo settings page renders two sections:

```
Members of org/acme/heddle
─────────────────────────
Direct grants (3)
  alice    Maintainer    [edit] [remove]
  bob      Developer     [edit] [remove]
  carol    Reader        [edit] [remove]

Inherited from org/acme (2)              [open namespace settings →]
  dave     Admin
  erin     Maintainer
```

Inherited rows have no edit/remove affordance on the repo page — the
maintainer must follow the link up to the namespace to manage them.
This mirrors how GitHub surfaces organisation-vs-repo membership and
sidesteps any "deletes the wrong thing" UX bug.

---

## §4 Missing RPCs + proto bumps

Summary of the new wire surface introduced by this spike:

| RPC | Status | Notes |
|---|---|---|
| `CreateGrant` | exists (proto:25) | reused by §3.1 |
| `CreateInvitation` | extend | §3.1 — swap `namespace_path` for `GrantTargetRef target` so repo-scoped invites stay repo-scoped (wire-breaking on the one field; covered by the 0.7 → 0.8 bump) |
| `ListGrants` | extend | add `include_inherited` field + `source_resource` on `HostedGrant` (§3.3) |
| `GetMyEffectiveRole` | **new** | §3.2 — any signed-in user; trigger for "Request access" UI without needing the admin-gated `ListGrants` |
| `RequestRoleGrant` | **new** | §3.2 — user asks for access; `requested_role != UNSPECIFIED` enforced |
| `ListPendingGrantRequests` | **new** | §3.2 — maintainer inbox |
| `RespondToGrantRequest` | **new** | §3.2 — approve/deny; `granted_role != UNSPECIFIED` enforced when `approve=true` |
| `DeleteGrant` | exists (proto:28) | revoke is already covered |

**Additional message-level changes:**

- `RoleGrantRequest.status` is typed as the new
  `RoleGrantRequestStatus` enum, not a free string (§3.2).
- `HostedGrant` gains `google.protobuf.Timestamp expires_at`
  (additive, NULL-on-existing) to support temporary grants minted
  via `RespondToGrantRequest` (§3.2).

**heddle-grpc proto bump:** 0.7 → 0.8 (semver-minor: additive RPCs +
additive message fields, no breaks). Three consumers track this
proto — heddle, weft, tapestry — and each needs a coordinated
version bump and codegen refresh, same workflow as the
0.6 → 0.7 bump in heddle#226 / weft#228.

**Database migrations (weft):**

- New table `pending_grant_requests` with the partial-unique
  `WHERE status = 'pending'` index from §3.2 and a TTL index
  supporting the expiry sweep.
- `ALTER TABLE invitations` adding `target_kind` + `target_path`
  columns (and a back-fill that maps existing rows'
  `namespace_path` onto `(target_kind='namespace', target_path=…)`).
- `ALTER TABLE grants ADD COLUMN expires_at TIMESTAMPTZ NULL` to
  back the new `HostedGrant.expires_at` field and the temporary-
  grant path in `RespondToGrantRequest`.

---

## §5 Sub-impl issues to file post-merge

(Filed after this spike merges — not in this PR.)

- **weft:** implement `GetMyEffectiveRole`, `RequestRoleGrant`,
  `ListPendingGrantRequests`, `RespondToGrantRequest` + the
  `pending_grant_requests` table (with the partial-unique
  pending-only index). Includes the per-IP + per-requester
  anti-spam rate-limit hookup (extend `PerIpRateLimitLayer`'s
  limit class) and the `requested_role` / `granted_role`
  `UNSPECIFIED`-rejection validation.
- **weft:** extend `CreateInvitation` to accept
  `GrantTargetRef target`; migrate `invitations` table; update the
  redeem handler to mint a grant on the stored target kind.
- **weft:** add `expires_at` column to `grants`, populate
  `HostedGrant.expires_at` on reads, honour it on
  `RespondToGrantRequest` writes, and add a background sweep that
  revokes expired grants. Land in lockstep with
  `RespondToGrantRequest` so the wire field never has a no-op
  window.
- **weft:** extend `ListGrants` with `include_inherited` +
  populate `HostedGrant.source_resource` on the inheritance walk.
- **weft:** background sweep that flips `pending_grant_requests`
  rows past `expires_at` to `status=expired`.
- **weft (substrate hardening):** add `require_user_subject_for_write`
  helper + parameterised integration test that loops the
  grant-mutation RPC surface asserting anon → `failed_precondition`
  (the §2 follow-up).
- **heddle-grpc:** 0.8 minor bump — new RPCs + additive fields,
  codegen, version bump in `crates/grpc/Cargo.toml` and the three
  consuming Cargo manifests.
- **tapestry:** invite UI (handle / email picker, role dropdown
  clamped to caller's role).
- **tapestry:** request-access UI on repo + namespace pages for
  signed-in users without a grant.
- **tapestry:** maintainer inbox showing pending requests with
  approve/deny + counter-offer-role affordance.
- **tapestry:** inheritance-surface UI on repo-settings (direct vs
  inherited sections per §3.3).
- **e2e tests:** one Playwright scenario per flow
  (invite-by-email-then-redeem, request-then-approve, inherited-grant
  surfaces with correct attribution and is read-only on the repo
  page).
- **heddle (consumer):** heddle#27 CONTRIBUTING.md updates citing
  the three flows from this spike.

---

## §6 Out of scope

The following are deliberately excluded from this spike and any v0
implementation that follows:

- **Notification delivery for grant events.** No email / Slack /
  webhook firing on grant create, request, approve, deny, or
  invitation expiry. v0 surfaces state on next page load. The
  existing oplog already records the mutation for audit; the
  separate question of *push* notifications is its own design.
- **Audit logging beyond the oplog.** The existing
  hosted-registry oplog captures grant mutations; no new audit
  pipeline.
- **Off-instance contributor identity verification.** No SSO,
  no third-party org bridging (GitHub teams, Google Workspace
  groups, SCIM). Identity comes from the existing
  `subject_user_uuid` resolution path. A future federation story
  reuses `CreateGrant` — it doesn't need a new substrate.
- **Anti-abuse rate-limiting beyond per-requester-per-resource
  and per-IP.** Spam mitigation at the "create thousands of
  accounts and request access to everything" tier is a separate
  abuse-handling design — out of scope.
- **A "guest contributor" tier between anon and hosted account.**
  There isn't one. Anon = read-only-public. Authorized = hosted
  account + role grant. Full stop. This was the framing error
  the closed PR #241 made; calling it out explicitly so a future
  reader doesn't re-introduce it.
- **Anon writes of any kind.** Per §2, the substrate rejects
  anon callers from write RPCs via `require_user_subject`. The
  spike does not loosen that gate and the impl issues in §5 must
  not either.
