# HYBRID Part 9: receiver owner pin and API alpha.39

All workspace and standalone verifier API dependencies select
`=0.31.0-alpha.39`, git revision `d37de5cf154749747a9632cbce839c4348f17762`.
There is one wire contract and no compatibility reader.

For the Spool owner's own writes, `WitnessedAuthors::resolve` selects the
statement's independently pinned Spool owner, requires that verified state to
extend the envelope history, and checks the attested attachment against the
pinned state's retained issuers. Recover therefore cuts earlier device
attachments even when an incoming envelope omits the Recover. Genesis, P2,
landing requests and boundary acceptances share this resolution. Non-owner
writers keep their own verified witnessed history and attachment inventory.
The portable Genesis byte/WASM seam applies the same owner pin rule.

`NativeAuthorityContext` now accepts only `WitnessedAuthors`; a host's durable
inventory cannot substitute through a common authority trait. Its construction
requires resolved witness evidence, matching payload commitments and verified
original or acceptance signatures. The browser seam documents its independently
authenticated author-history/enrollment inputs and validates every supplied
attachment, including unused entries; duplicates, forged entries and recovered
issuers refuse. Signatures alone do not establish enrollment.

Landing no longer compares a bound import original's publisher with that same
original. Carrier verification already binds it to the delegation job key.
Native LocalKey work still requires its exact ownership frontier.

Fetch staging and transactional foreign-prefix replay share a traversal budget:
32 foreign edges deep and 256 uncached prefixes across the entire traversal,
including sibling branches. Cycles remain Scope refusals. Exhaustion has the
typed `ForeignPrefixLimitExceeded` error, propagated through the hosted protocol
boundary. Budgets are local traversal limits, independent of history paging.

The committed `scripts/prove-native-witness-guards.py` contains all 17 Part 8
mutations plus the new receiver-pin mutation. API mutations relocate only an
isolated copy of the selected API dependency. Runtime assertion failures and
successful nonempty restored selections are required; compiler failures count
as failures of the proof script. `scripts/regenerate-hybrid-alpha33.py` now
regenerates the foreign, writer and boundary corpora as well as the original
HYBRID corpora, including deterministic sealed capability seed inputs. Generated
signed records are compared with the current API pin; historical descriptor
hash manifests do not describe this release. No signed fixture was hand-edited.

## Surfaces and alpha.38/39 consumer audit

The CLI verb/flag/help surface is unchanged. Human and agent replication errors
retain the typed traversal refusal. Git import/export/projection continues
through sley. Recover refusal and retained-after-Rotate controls cover the
reverse authority states. The changed wire types come from heddle-api.

Heddle does not interpret Spool invitation recipient/state fields, notification
EffectiveDelivery cells, or approval-group member rosters in its CLI/daemon.
Spool/notification events flow through the shared typed observation contract;
all unary RPCs are available through the shared v2 client. No local field shim
or second view RPC was added. Existing signup-invitation status is a different
IdentityService contract and retains its `revoked`/`redeemed` flags. Hosted error
conversion preserves the complete new invitation/approval refusal reasons and
fields; a regression exercises all four new reasons.

## Coordinated changes weft must adopt

Heddle verifier interfaces:

- `NativeAuthorityContext<'a>` has no authority-type parameter and requires
  `author_authority: &'a WitnessedAuthors`.
- `WitnessedAuthors::resolve(account, envelope, pinned_owner, now_seconds)` takes
  the statement's selected owner. `HostAuthorAuthority` has its own inherent
  resolver; the common `AuthorAuthority` trait is removed.
- `VerifiedOwnerState::extends` is public. Spool-owner writer verification uses
  the pinned state and its retained issuer map; never replace it with the
  envelope's own endpoint. Non-owner account resolution remains independent.
- `ForeignPrefixBudget` and typed `ForeignPrefixLimitExceeded` cover both
  staging and replay, with one count budget shared across sibling recursion.
- `native_genesis::verify_bytes`/`verifyNativeGenesisAuthority` require
  independently authenticated attachment enrollment or verified exact witness
  testimony, and strictly validate all inventory entries and the owner pin.

Alpha.38/39 hosted API hard cuts:

- `InvitationRecord.recipient` is a required email/handle/account_id oneof;
  `state: InvitationState` replaces revoked/redeemed. New inviter, agent-label,
  timestamp and Spool display fields must use the shared private projection.
  Create yields a secret only for email; Resolve/Redeem are email-only.
- Implement caller-bound, human-session `AcceptInvitation`/`DeclineInvitation`.
  Current ADMINISTRATOR authority is required for every offered role at Create,
  pending Accept, Redeem and code read. Authority loss revokes all pending
  invitations. Accepted retries are recipient-bound no-ops, without restoring
  grants or checking former inviter authority. Decline notifies once.
- Notifications and attention carry invitation records and new invitation kinds
  and action capabilities. `EffectiveDelivery.source` includes INHERITED and
  ACCOUNT; `source_spool` identifies only readable winning ancestors. Implement
  bounded inheritance, `effective_delivery_spool` and explicit
  `clear_unreadable_scopes`, preserving hidden rules by default.
- `ApprovalGroupRecord.explicit_member_handles` replaces `principal_ids` without
  a shim. `member_role` selects live effective-role members. Reads use
  `ApprovalGroupView`, with ID-free visible members, role-selected handles and
  private counts; writes still use `ApprovalGroupRecord`. Reevaluate membership
  atomically at landing and enforce the WRITER eligibility floor.
- Implement scoped `SuggestPrincipals` with the two-scalar minimum, 20-result
  bound, member-only scope authorization, privacy and account rate limiting.
- Implement `GetSignupInvitationCode` and `GetInvitationCode` as authenticated,
  proof-bound creator reads. Return the original code only while pending and
  unexpired; terminal and legacy/non-link invitations return empty. Other
  principals receive uniform NOT_FOUND. Bind encrypted-at-rest storage to the
  immutable creator/reference/purpose/deployment, destroy ciphertext on terminal
  transitions, serialize reads with transitions, pace and audit without secrets,
  and exclude codes from projections, receipts, caches and traces.
- Preserve `INVITATION_HUMAN_SESSION_REQUIRED` (204),
  `INVITATION_INVITER_AUTHORITY_LOST` (205),
  `INVITATION_HANDLE_NOT_FOUND` (302), and
  `APPROVAL_ROLE_BELOW_ELIGIBILITY_FLOOR` (403) with their shared fields/call codes.

These are coordinated host obligations, not a claim of a live weft deployment.

## Verification

Final gate results and exact fail-then-pass receipts are recorded here after
execution. Each gate uses a fresh `HEDDLE_HOME`, `TMPDIR=/home/scratch` and
`CARGO_TARGET_DIR=/runner/heddleco-build/scratch/heddle-part9-target`.
