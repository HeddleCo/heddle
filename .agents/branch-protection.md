# Branch Protection

> Stack policies that gate `merge` (and `force_push`, `complete`)
> on multi-party approval. Most-restrictive wins.

## Mental model

A **thread policy** is a row in `thread_policies` that says:
"merging into threads matching this glob requires N approvals
from people with role R, plus M approvals from group G when the
diff touches these paths."

When the CLI or web asks "can `<source>` merge into `<target>`?",
the server pulls every policy whose `target_thread_pattern`
glob-matches `<target>`, evaluates each one's requirements
against the recorded approvals, and returns:

- `allowed: true` when every triggered policy is satisfied, OR
- a list of `unmet[]` requirements with reasons.

Multiple matching policies on the same target deliberately stack:
their requirements **AND** together. If one policy says "1
maintainer" and another says "1 admin", you need both.

## The four moving parts

```
┌──────────────────┐    ┌─────────────────────┐
│  thread_policies │    │  approval_groups    │
│  (admin-only)    │    │  (admin-only)       │
└────────┬─────────┘    └──────────┬──────────┘
         │                         │
         │ ──> policy_group_       │ <── approval_group_members
         │     requirements        │     (rule-based: "anyone
         │     (path-conditional   │      with role R on ns N
         │      group N-of-M)      │      or any ancestor")
         ▼                         ▼
              ┌───────────────────┐
              │ thread_approvals  │  <── approver-driven
              │ (any reader+ can  │      (forbid_author_approval
              │  add)             │       opt-in)
              └───────────────────┘
```

| Table | Lifetime | Owner |
|---|---|---|
| `thread_policies` | Long-lived; admin-managed | Admin on the repo's parent namespace |
| `policy_group_requirements` | Hangs off a policy | Same admin |
| `approval_groups` + `approval_group_members` | Long-lived; reusable | Same admin |
| `thread_approvals` | Per-merge; volatile | The approver themselves (or repo admin to revoke) |

## Stacking semantics

Two policies on `target_thread_pattern = "main"`:

```text
Policy A: required_approvals = 1, required_role = "maintainer"
Policy B: required_approvals = 1, required_role = "admin"
```

Outcomes:

| Approvers | A satisfied? | B satisfied? | Gate |
|---|---|---|---|
| 1× maintainer | ✓ | ✗ (needs admin) | **blocked** |
| 1× admin | ✓ (admin > maintainer) | ✓ | **open** |
| 1× maintainer + 1× admin | ✓ | ✓ | **open** |

The role lattice is `reader < developer < maintainer < admin <
owner`. An approval from someone whose effective role on the
repo is *at least* the required role counts toward that policy's
flat-role requirement.

## Group membership is lazy

`approval_group_members` rows are *rules*, not memberships:

```text
group_id        | namespace_path | required_role
core-maintainers | org/acme       | maintainer
core-maintainers | org/acme/sec   | admin
```

At gate-eval time the server walks the grants graph: anyone with
maintainer-or-higher on `org/acme` (or anyone with admin-or-higher
on `org/acme/sec`) is treated as a member of `core-maintainers`.
Grants on **ancestors** cascade — `harness.hosted_registry
.grant_namespace("alice", Maintainer, "org/acme")` makes Alice a
member even when the rule talks about `org/acme/team`.

This means:

- **Adding a member** = grant them the role. No second step.
- **Removing a member** = revoke the role. Their approval stops
  counting on the next gate query, no cache invalidation.
- **Group membership tracks reorgs.** Promote alice from
  developer → maintainer and she immediately starts qualifying
  for any group rule that wants maintainer+.

## Path triggers

Two layers of path-conditioning, both CODEOWNERS-style globs:

- `thread_policies.required_paths` — the *whole policy* fires
  only when the diff intersects one of these. Empty list = "fires
  on every change."
- `policy_group_requirements.only_if_paths_match` — *that
  specific group requirement* fires only when the diff
  intersects. Empty = always fires.

So you can write: "every merge to main needs 1 maintainer
approval; if the diff touches `crates/crypto/**`,
*also* needs 1 approval from the `security-reviewers` group."

If the caller passes `changed_paths: []` (we don't know what the
diff touches), we fail-closed: the policy and every group
requirement is treated as triggered. Pre-merge UIs should always
pass the diff path list; the gate UI ships with that data
already.

## Glob syntax

Hand-rolled in `server/src/access/glob.rs`. Supports:

| Pattern | Matches |
|---|---|
| `*` | Any chars except `/` |
| `**` | Any chars including `/` (zero or more segments) |
| `?` | Exactly one non-slash char |
| literal | itself |

```text
"main"                 → "main" only
"release/*"            → "release/v1" but NOT "release/v1/rc"
"release/**"           → "release", "release/v1", "release/v1/rc"
"crates/**/Cargo.toml" → "crates/Cargo.toml", "crates/x/Cargo.toml",
                         "crates/x/y/Cargo.toml"
"v?"                   → "v1", "v9"; NOT "v" or "v10"
"*"                    → every single-segment thread name
```

Same matcher used for both target threads (`target_thread_pattern`)
and paths (`required_paths`, `only_if_paths_match`).

## Approval lifecycle

```
[ alice runs `heddle thread approve feat/x main` ]
       │
       │  CLI reads source_state from local repo
       │  (the change_id at feat/x's head)
       ▼
[ INSERT INTO thread_approvals
    (repo, source, target, source_state,
     approver_user_id, expires_at) ]
       │
       │  expires_at = NOW() + min(matching policies' approval_ttl_secs)
       │  approver_sid = the Biscuit session id (audit)
       ▼
[ gate counts this approval until ANY of:
   - the row is expired (wall clock past expires_at), OR
   - source_state changes AND any matching policy
     has stale_on_update = true (push invalidation), OR
   - a policy with forbid_author_approval=true lists
     alice as the source thread's author, OR
   - the row is revoked via RevokeApproval ]
```

`forbid_author_approval` only fires when the gate query passes
`author_user_id`. The pre-merge UI should pass it (the source
thread's most-recent author); the CLI doesn't pass it today, so
that gate is conservative-by-default off in raw `check-merge`
calls. (Plumb this in when wiring the merge UI.)

## Operator UI

`/app/repo/<path>/branch-protection` (admin-only). Backed by
`HostedAdminService::{Create,List,Delete}ThreadPolicy` plus the
group + requirement RPCs.

For the per-merge approver flow, use the CLI for now:

```bash
# Record an approval
heddle thread approve feat/x main

# See what's been recorded
heddle thread approvals feat/x main

# Query the gate
heddle thread check-merge feat/x main --path crates/crypto/src/ed25519.rs

# Take it back
heddle thread revoke-approval <uuid>
```

Exits non-zero on `check-merge` when the gate is closed — useful
in pre-merge hooks.

## Designing a policy

Reach for **flat-role** when the rule is "this many people of
this rank, anywhere." Reach for **group requirements** when the
rule names a specific cohort ("the security team"). Reach for
**path triggers** when the rule depends on what changed.

Three real-world examples:

```text
1. "Two maintainers must approve any merge into main."
   target_thread_pattern: "main"
   gated_action:          "merge"
   required_approvals:    2
   required_role:         "maintainer"

2. "Release branches need an admin sign-off."
   target_thread_pattern: "release/*"
   gated_action:          "merge"
   required_approvals:    1
   required_role:         "admin"

3. "Anyone touching crypto code needs security-team approval."
   target_thread_pattern: "main"
   gated_action:          "merge"
   required_approvals:    0                         # no flat-role gate
   policy_group_requirements:
     - group:             security-reviewers
       required:          1
       only_if_paths:     ["crates/**/biscuit/**",
                           "crates/crypto/**"]
```

Stack all three on `main` and you get: 2 maintainers always, plus
1 admin on release branches, plus 1 security reviewer when the
diff touches crypto. Most-restrictive cumulative.

## Server-side touchpoints

The hosted server moved to the sibling **weft** repo; the paths below are
relative to that repo's server crate.

| Concern | File |
|---|---|
| Glob matcher | `src/access/glob.rs` |
| Gate evaluator | `src/access/merge_gate.rs` |
| Policy + group SQL | `src/pg_registry.rs` |
| Admin RPC handlers | `src/server/grpc_hosted_impl/admin.rs` |
| Approval RPC handlers | `src/server/grpc_hosted_impl/user.rs` |
| Schema | `migrations/006_thread_policies.sql` |

The whole role-inheritance + group-membership lattice fits in two
helpers (`role_satisfies`, `namespace_covers`) and a recursive CTE
in `pg_registry::group_membership_for_subject`. If you find
yourself adding bespoke role logic somewhere else, refer back to
those two — there's a strong bias toward keeping the lattice in
one place.

## Gotchas

- **`forbid_author_approval` needs `author_user_id`.** Without
  that hint, the gate can't enforce the rule and the approval is
  let through. Wire `author_user_id` whenever you call
  `CheckMergeEligibility` from a context that knows the author
  (the merge UI, pre-merge hooks).

- **Empty `changed_paths` is fail-closed.** Path-conditional
  triggers fire when we don't know the diff. Pass `vec![]` only
  when you genuinely don't know; otherwise pass the actual
  change set. The CLI's `heddle thread check-merge` defaults to
  empty unless you pass `--path`.

- **Approvals re-record idempotently.** Re-approving the same
  `(repo, source, target, source_state, approver)` updates the
  existing row in place rather than inserting a duplicate. The
  unique constraint matters for the gate — duplicates would
  count once, not twice, but cluttering the listing UI is
  pointless.

- **Revoking an approval reopens the gate.** Tested explicitly in
  `revoke_approval_removes_it_from_listing_and_gate`.

- **`stale_on_update` is per-policy.** Two stacked policies can
  disagree — one stale-on-push, one not. The gate evaluates each
  independently, so an approval can satisfy the one that's
  permissive while failing the one that's strict.

- **Schema renumbered after PR 5d follow-up.** Migration 006
  dropped `forbid_self_approval`; field tags 8–12 in the
  `ThreadPolicy` proto shifted down. Clean cutover, no migration
  needed for prod (no prod yet).
