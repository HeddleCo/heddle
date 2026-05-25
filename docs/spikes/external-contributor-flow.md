# heddle#240 — external-contributor flow spike

**Status:** spike (decision doc). Implementation tracked in follow-up
issues — see §5.
**Scope:** the UX path for a contributor who reviews / signs / submits
against a Weft instance **without ever creating a Weft account**. The
documentation it unblocks lives in heddle#27.

> Sibling substrate doc: `weft/docs/auth/anon-biscuit-spike.md`
> (weft#189). That spike picked the auth substrate. This spike picks
> the *contributor-facing* UX layered on top of it.

---

## §1 Current state

What an account-less browser can already do, grounded against current
weft + tapestry code.

### 1.1 Tapestry mints an anon biscuit on first touch

`tapestry/src/hooks.server.ts:67-89` calls
`api.auth.mintAnonBiscuit()` on any GET HTML navigation that arrives
without a `weft_auth` cookie and writes the response back as an
httpOnly+SameSite=Lax cookie. The cookie value **is** the biscuit
(base64url). On every subsequent request `hooks.server.ts:44-47`
forwards it as `Authorization: Bearer` to weft. Webhook POSTs are
deliberately *not* minted at the hook layer (DoS amplification —
`hooks.server.ts:13-22`).

The contributor never sees a sign-in wall to reach the review surface.
A maintainer who pastes a tapestry URL into Slack hands the recipient
a usable biscuit by the time the page renders.

### 1.2 Verifier stamps `Subject::Anon` / `Subject::User`

The biscuit verifier (`weft/crates/weft-server/src/biscuit.rs:91-95`,
elaborated in `weft/docs/auth/anon-biscuit-spike.md` §3.1) injects a
`VerifiedBiscuit { subject, revocation_id, expires_at, tier_facts }`
extension on every authenticated request. `Subject` is a two-variant
enum: `User(Uuid)` or `Anon(Uuid)`.

Authorization helpers in
`weft/crates/weft-server/src/server/grpc_hosted_impl/auth_helpers.rs`
gate per-RPC:

- `require_anon_subject` (l.167) — admit only anon callers; reject
  user with `FailedPrecondition`.
- `require_user_subject` (l.186) — admit only user callers; reject
  anon with `FailedPrecondition`.
- `require_verified_biscuit` (l.33) — admit either; reject missing
  bearer with `Unauthenticated`.

### 1.3 What anon biscuits can actually call today

Walking the gRPC surface (`weft/proto/heddle/v1/service.proto`) against
those gates:

| RPC | Gate | Anon? |
|---|---|---|
| `MintAnonBiscuit` | `require_anon_subject` at the entry point of `LinkOAuthIdentity` (`grpc_hosted_impl/auth.rs:1836-1840`) is the promotion-only gate; mint itself is unauthenticated | yes |
| `LinkOAuthIdentity` (`service.proto:90`) | `require_anon_subject` — promotion path only | anon only |
| `AnalyzeExternalDiff` (`service.proto:92`) | per-IP + per-anon-uuid throttle in `middleware/per_ip_rate_limit.rs:359-367` (`5/h/IP`, `1/24h/anon-id`), no subject gate | yes, throttled |
| `SignState` (`service.proto:2363`), `ListSignatures`, `GetReviewPayload` | `open_repo_for_bearer` → `authorize_subject_with_claims` → `resolve_repository_access(&user.username, repo_path)` (`hosted_access.rs:144-220`) | **no** — anon UUID has no `User` row; `resolve_repository_access` returns `AuthorizationFailed` |
| `OpenDiscussion`, `AppendTurn`, `ResolveDiscussion`, `ListByState`, `ListBySymbol`, `GetDiscussion` (`service.proto:2547-2552`) | same `open_repo_for_bearer` shape in `grpc_hosted_impl/discussion.rs:235,320,393,472,506,536` | **no** — same gap |
| `Push` / `Pull` / `ListRefs` / `UpdateRef` (`service.proto:9-12`) | same `open_repo_for_bearer` shape in `sync.rs` | **no** |
| WebAuthn / device-flow RPCs (`service.proto:73-89`) | mint paths — gate is presence + envelope, not user-bound | yes |

**The substrate gap:** the *auth* layer admits anon, but the *repo
access* layer (`open_repo_for_bearer`) looks up the subject as a
username and demands a hosted role row. So anon callers reach SignState,
OpenDiscussion, etc. and get a `PermissionDenied` from
`resolve_repository_access`. This is the headline finding of §1.

### 1.4 What tapestry exposes pre-authentication

`tapestry/src/lib/components/review/SigningFooter.svelte:11-35` already
loads a per-subject Ed25519 device key from IDB
(`tapestry/src/lib/client/device-key.ts:26-265`) and signs the
canonical payload with `crypto.subtle.sign('Ed25519', …)`. The key is
stored keyed by `subject` (`device-key.ts:215-265`). On a hosted user
the subject is the Heddle username; on an anon biscuit there is no
username plumbing through to the footer today, so even though the IDB
substrate is per-subject, the *contributor-facing* sign affordance has
no anon code path.

### 1.5 Summary

Substrate-wise, anon biscuits are first-class. UX-wise, the *unprivileged
mint + verify* loop exists end-to-end; the *participate in a review /
discussion / contribution* loop is gated behind the per-repo hosted-role
check inside `open_repo_for_bearer`, which has no concept of a guest.

---

## §2 Flow design

The design goal: a guest can land on a review URL, leave a signed
comment, sign a review, and submit a small contribution — all without
clicking "Sign up" once. Everything below is the proposed UX shape;
the impl issues that build it live in §5.

### 2.1 Landing → biscuit → identity stub

1. Guest opens a tapestry review URL (`/r/<state_id>` or equivalent).
2. `hooks.server.ts` mints `Subject::Anon(uuid)` and sets `weft_auth`
   (already happens — §1.1).
3. The review surface renders read-only. Discussions, signatures, and
   the diff are visible. *(Today: the read RPCs themselves 401; see
   §5 / impl issue to relax `open_repo_for_bearer` for read on
   repos that have opted into guest access.)*
4. A `Sign in as guest` affordance sits beside the existing `Sign in`
   button on the SigningFooter and the discussion-compose box. Clicking
   it does not redirect — it expands a small panel inline.

### 2.2 First sign as a guest

On first guest-sign in this browser:

1. The panel asks for a display name (free text, no email). Suggestion
   placeholder: `Guest from <approx-geo-from-cf-ip>`.
2. On submit, tapestry generates an Ed25519 keypair in WebCrypto and
   calls `saveDeviceKey(subject, key)` using the **anon biscuit's
   subject UUID** as the IDB key — extending the existing per-subject
   schema from `device-key.ts:215-265` to anon subjects unchanged.
3. Tapestry stamps the guest display name into a new biscuit fact
   (`guest_display_name($name)`) via a new
   `RegisterGuestIdentity(name, public_key) → AccessTokenResponse` RPC,
   which atomically (a) writes a server-side `guest_pubkeys` row
   binding the anon UUID → pubkey + display name, (b) re-mints the
   biscuit with the extra fact + the pubkey bound via a grant envelope
   (tapestry#12-shaped — the existing envelope path), and (c)
   `Set-Cookie`s the new biscuit.
4. Subsequent navigations carry the guest-identified biscuit. No
   account row in `users`. No OAuth round-trip.

### 2.3 Signing a comment / signature

`SigningFooter.svelte` already does Ed25519-over-canonical-payload for
hosted users. The guest path reuses the same code with the guest-bound
keypair from §2.2:

1. `loadDeviceKey(subject)` (`device-key.ts:249-265`) — subject is the
   anon UUID — returns the keypair.
2. `crypto.subtle.sign('Ed25519', …)` signs the SignState canonical
   payload.
3. POST to `SignState` (`service.proto:2363`).
4. The server records the signature with `actor_name = guest display
   name`, `actor_email = ""`, plus a new `actor_kind = guest`
   discriminant on the `ReviewSignature` proto (§5).

Same shape for `OpenDiscussion` / `AppendTurn`: the existing RPC writes
a `DiscussionTurn { author_name, author_email, body, posted_at }`
(`service.proto:2444-2448`). The guest path writes `author_name = guest
display name`, `author_email = ""`, and the new `author_kind = guest`
discriminant flows through to the surface that renders the turn.

### 2.4 Maintainer-visible affordance

A guest signature renders with a distinct chip — `guest` — beside the
display name, plus a hover card showing:

- Pubkey fingerprint (first 8 hex of SHA-256, formatted as
  `gp_xxxxxxxx`).
- "First signed on this repo: `<date>`" — derived from the
  `guest_pubkeys` row plus the first signature joined to that pubkey.
- Whether the same pubkey has appeared on other states in this repo
  (count). Cross-repo aggregation is **out of scope** (§6).

Hosted signatures keep their existing chip / verified-identity hover.
The visual contrast is the trust signal — the maintainer is told "this
is unverified, decide accordingly."

### 2.5 Submitting a contribution

A guest contribution flows through `Push` (`service.proto:11`):

1. Guest forks the repo locally with `heddle clone <url>` (no auth) →
   `heddle commit` (no auth).
2. Guest opens the contribution surface (a new tapestry route) and gets
   prompted to sign in. The flow above runs (anon → guest with display
   name + pubkey).
3. Tapestry mints a *contribution-scoped* biscuit attenuated to push
   onto a guest-fork ref (`refs/contrib/<guest-pubkey-fp>/<branch>`),
   not the maintainer's branch namespace. The biscuit is short-lived
   (1 hour) and IP-bound via `tier_facts`.
4. The CLI receives the biscuit through a `heddle pair` device-flow
   handshake (`service.proto:77-80`, `CreateDeviceAuthorization` family
   already exists).
5. `heddle push` runs against the guest-fork ref namespace. The
   maintainer sees the inbound branch on the review surface with the
   same `guest` chip.

The maintainer accepts the contribution by signing it themselves
(SignState) and merging the guest-fork ref into the canonical thread.
The guest's commits retain their own signatures; the merge is the
trust delegation.

### 2.6 Cross-device participation

By construction the guest pubkey lives in IDB on one browser. Picking
up the same guest identity on a different device is **explicitly not
supported by default** — see §3 for why and §6 for what it would take.
A guest who signs on their phone is, from the server's point of view,
a different guest than the one who signed on their laptop.

A "sign in as the same guest on another device" affordance can be
layered later by binding a passkey at guest-register time (§5 has the
impl issue). The default flow stays passkey-free.

---

## §3 Identity persistence — decision

Candidates considered:

| Candidate | Pro | Con |
|---|---|---|
| **A. Device-key in IDB (proposed)** | Reuses existing tapestry#12 / `device-key.ts` substrate verbatim. No new account state. No phone-home. | Browser-storage-scoped: clearing site data = lost identity. No cross-device. |
| B. Mandatory passkey at guest-register | Cross-device, durable. | Forces a platform authenticator UX cliff before the first comment. Many guests bounce. Defeats "no account" framing. |
| C. Mandatory email + verification | Durable, dedupable. | Defeats "no account" framing entirely. Telemetry-equivalent. |
| D. Fully anonymous, no persistent guest identity | Zero friction. | Every refresh churns the pubkey; maintainers can't tell whether two signatures are from the same person. Trust signal is gone. |

**Decision: A (Device-key in IDB).** Rationale:

- Substrate already exists (`device-key.ts:26-265`,
  `SigningFooter.svelte:11-35`). Anon UUID slots into the existing
  per-subject schema with no schema change client-side.
- "No Weft account" is preserved literally — no row in `users`, no
  OAuth, no email.
- The trust degradation vs. hosted users is in the right place:
  maintainers see the `guest` chip (§2.4) and make their own call.
- Cross-device is a layerable upgrade (§5), not a blocker.

The IDB-cleared / new-device case degrades to candidate D for that
device — they re-register as a new guest. Maintainers can either
re-acknowledge them or refuse — the trust signal correctly reflects
"this is a fresh, unverifiable participant".

### 3.1 What the server has to add

- Migration: `guest_pubkeys (anon_uuid uuid pk, public_key bytea,
  display_name text, created_at)`. Lives in weft.
- New RPC: `RegisterGuestIdentity(display_name, public_key) →
  AccessTokenResponse`. Gated by `require_anon_subject`. Idempotent on
  `(anon_uuid)` — second call rotates the display name + pubkey,
  re-mints the biscuit, and is rate-limited (`per_ip_rate_limit.rs`
  pattern).
- Read-side join: `ReviewSignature` and `DiscussionTurn` carry a
  `guest_pubkey_fp` field; the renderer looks it up to compute the
  `gp_xxxxxxxx` chip.
- `open_repo_for_bearer` relaxation: when the request is read-only
  *and* the repo's hosted record sets `guest_access = read` (new
  column), admit anon callers. When it's a write to a *guest-fork
  ref namespace*, admit any guest with a registered pubkey.

---

## §4 Documentation outline (heddle#27)

Sketch of what the CONTRIBUTING.md PR for heddle#27 will say. Bullets,
not prose:

- **You don't need a Weft account to contribute.** What you *do* need:
  a browser (to register a guest identity) and `heddle-cli` (to push).
- **Quick path — leave a comment / sign a review.** Open the review
  URL → click `Sign in as guest` → pick a display name → comment or
  sign. The site stores your signing key in this browser only.
- **Standard path — submit a small fix.** `heddle clone <url>` →
  `heddle commit -m '…'` → open the contribution URL → register as
  guest if you haven't → `heddle pair` to get a contribution-scoped
  credential → `heddle push`.
- **What the maintainer sees.** Your display name with a `guest`
  chip and a `gp_xxxxxxxx` pubkey fingerprint. They sign your
  contribution themselves to merge it; their signature is the
  trust delegation.
- **Persistence and limits.** Your guest identity lives in your
  browser. Clearing site data resets it. Cross-device support
  requires the optional passkey upgrade (linked).
- **No telemetry.** No email, no analytics, no phone-home for guests.
  Rate limits apply at the per-IP and per-anon-UUID tiers
  (`5/h/IP` for diff analysis, etc.).
- **When you might want an account anyway.** Long-running
  contributions, multi-repo work, persistent reputation, cross-device
  identity. Link to the hosted-account onboarding doc.

---

## §5 Sub-impl issues to file (after this spike merges)

File against the appropriate repo. None of these land in this PR.

- **weft:** `guest_pubkeys` table + `RegisterGuestIdentity` RPC
  (`service.proto`, migration, handler under `grpc_hosted_impl/auth.rs`,
  envelope-binding of the pubkey via tapestry#12 grant-envelope path).
- **weft:** relax `open_repo_for_bearer` (`hosted_access.rs:144-220`)
  to admit `Subject::Anon` on (a) reads against repos with `guest_access
  = read`, (b) writes scoped to a guest-fork ref namespace.
- **weft:** add `actor_kind` / `author_kind` discriminant +
  `guest_pubkey_fp` field to `ReviewSignature` / `DiscussionTurn` in
  `service.proto`, and write through in `state_review.rs:271-385` and
  `discussion.rs`.
- **weft:** add `guest_access` column to the hosted-repo record + UI to
  flip it (default off, opt-in per repo).
- **weft:** define the `refs/contrib/<guest-pubkey-fp>/<branch>`
  namespace + push-time validation (must match the caller's bound
  pubkey).
- **weft:** rate-limit policy for `RegisterGuestIdentity` and guest
  push: per-IP and per-anon-UUID tiers in
  `middleware/per_ip_rate_limit.rs`.
- **tapestry:** `GuestSignAffordance` component — the inline panel
  (display-name prompt + `crypto.subtle.generateKey` + `saveDeviceKey`
  + `RegisterGuestIdentity`). Slots into `SigningFooter.svelte` and
  the discussion compose box.
- **tapestry:** `guest` chip + `gp_xxxxxxxx` hover card on the
  signature list and discussion turn renderers.
- **tapestry:** contribution-flow route — connects `heddle pair` to
  the guest-identified browser session and mints the contribution-
  scoped biscuit.
- **heddle (CLI):** `heddle push` against a guest-fork ref namespace
  works end-to-end with the device-flow-issued biscuit.
- **heddle (CLI):** `heddle log` / `heddle review show` renders the
  `guest:` prefix and pubkey fingerprint for guest signatures.
- **heddle:** CONTRIBUTING.md per §4 (this is heddle#27).
- **tapestry (optional, post-MVP):** "Add a passkey to this guest
  identity" upgrade flow for cross-device portability — adds a passkey
  credential bound to the existing `anon_uuid + guest_pubkeys` row;
  does not promote to a hosted account.

---

## §6 Out of scope

Explicitly **not** addressed by this spike. Each gets its own ticket
if/when we want it:

- **Anti-abuse rate-limit shaping for anonymous-comment spam.** The
  existing per-IP + per-anon-UUID limits in `per_ip_rate_limit.rs`
  apply by inheritance; tuning for comment-spam specifically is a
  separate exercise once we have prod traffic.
- **Maintainer block / shadow-ban of a guest pubkey.** Likely a thin
  layer on `session_blocklist` (`migrations/001_identity.sql:191`), but
  out of scope here.
- **Cross-repo guest reputation.** A `gp_xxxxxxxx` pubkey is currently
  scoped to the repo where they first registered. Promoting it to
  org- or instance-wide reputation is a follow-up.
- **Off-board signature verification.** A third party verifying a
  guest signature without trusting the Weft instance (the pubkey is
  authoritative inside Weft today). Standalone proof bundles are
  future work.
- **Promotion guest → hosted user.** Currently a one-way demote:
  re-sign-in as a hosted user starts a separate identity. Merging
  prior guest activity into a freshly-claimed hosted account is a
  follow-up.
- **Guest identity portability across tapestry instances.** Same
  pubkey + different Weft = different `anon_uuid` and different
  `guest_pubkeys` row.
- **The hosted-account review/sign flow.** That's already shipped via
  the user-bearing path; this spike doesn't touch it.
