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
retain the typed traversal refusal and protocol exit classification, including
when Fetch adds error context. Git import/export/projection continues
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

Each gate uses a fresh `HEDDLE_HOME`, `TMPDIR=/home/scratch` and
`CARGO_TARGET_DIR=/runner/heddleco-build/scratch/heddle-part9-target`.

The regression-only checkpoint `85a5751474e4f768cf1dbb3b3d4f682da6835a3d`
is based on the landed Part 8 tree. It was reconstructed and executed separately
after the initial red/green run, and is preserved in this PR's ancestry. Its
runtime failure is the intended assertion, not a build failure:

```text
cargo test --offline --locked -p heddle-thread-api --lib \
  spool_owner_pre_recover_history_cannot_revive_cut_device -- --nocapture

receiver-pinned Recover must reject truncated history with the cut device attachment: ()
test hybrid::native_tests::writers::spool_owner_pre_recover_history_cannot_revive_cut_device ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 206 filtered out
exit 101
```

The implementation runs the same regression, including the current recovered
owner's passing control:

```text
test hybrid::native_tests::writers::spool_owner_pre_recover_history_cannot_revive_cut_device ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 206 filtered out
exit 0
```

The committed mutation runner independently removes the receiver pin at API
alpha.39. All 18 pairs ran on isolated source commit
`d1634c44a895c73e39354335994c9642e64f5cb1`. Each red exited 101 at a selected
runtime test; each restored green exited 0 with one passing test. The subsequent
CLI-only exit classification change leaves these authority guards unchanged.

| Guard | Red | Restored green |
| --- | --- | --- |
| spool-owner-receiver-pin | 101, failed | 0, 1 passed |
| governance-is-not-author | 101, failed | 0, 1 passed |
| foreign-resolver | 101, failed | 0, 1 passed |
| installed-status | 101, failed | 0, 1 passed |
| attachment-required | 101, failed | 0, 1 passed |
| attachment-exact-inventory | 101, failed | 0, 1 passed |
| actor-publisher-cut | 101, failed | 0, 1 passed |
| actor-mint-cut | 101, failed | 0, 1 passed |
| boundary-prefix-scope | 101, failed | 0, 1 passed |
| p4-own-admission | 101, failed | 0, 1 passed |
| root-owner-identity | 101, failed | 0, 1 passed |
| requester-token-binding | 101, failed | 0, 1 passed |
| p2-actor-binding | 101, failed | 0, 1 passed |
| recover-cuts-attachments | 101, failed | 0, 1 passed |
| writer-key-policy | 101, failed | 0, 1 passed |
| counterparty-policy | 101, failed | 0, 1 passed |
| review-only-p4 | 101, failed | 0, 1 passed |
| job-request-role | 101, failed | 0, 1 passed |

Reproduce all 17 Part 8 pairs and the new owner-pin pair from a committed tree:

```sh
python3 scripts/prove-native-witness-guards.py --output "$(mktemp -d)" \
  spool-owner-receiver-pin governance-is-not-author foreign-resolver \
  installed-status attachment-required attachment-exact-inventory \
  actor-publisher-cut actor-mint-cut boundary-prefix-scope p4-own-admission \
  root-owner-identity requester-token-binding p2-actor-binding \
  recover-cuts-attachments writer-key-policy counterparty-policy \
  review-only-p4 job-request-role
```

```text
ALL 18 FAIL-THEN-PASS PAIRS VERIFIED
```

All **41/41 final gates pass**. Required Rust commands executed **10,190 passing tests, zero failures, and 87 existing ignores/skips**. These are execution counts across overlapping configurations, not unique tests.

| Runtime gate | Passed | Ignored / skipped |
| --- | ---: | ---: |
| `affected-tests` | 1,356 | 11 |
| `workspace-tests` | 4,610 | 25 |
| `cli-serialized-units` | 505 | 0 |
| `cli-ci-integration` | 31 | 0 |
| `cli-ci-suite` | 2,095 | 41 |
| `hosted-client` | 489 | 3 |
| `thread-default` | 246 | 1 |
| `thread-signing` | 20 | 0 |
| `thread-root-attachment` | 4 | 0 |
| `thread-core` | 16 | 0 |
| `thread-native` | 198 | 0 |
| `thread-iroh-replication` | 107 | 1 |
| `semantic` | 336 | 3 |
| `thread-behavior` | 2 | 0 |
| `thread-transport-observation` | 1 | 0 |
| `repo-thread-matrix` | 93 | 2 |
| `objects-writer-lease` | 9 | 0 |
| `capability-wasm-tests` | 72 | 0 |

All formatting, affected/workspace/CI-feature clippy, Thread API feature clippy/checks, WASM builds, dependency boundaries, the runnable Thread example, release browser binding, and npm pack dry-run exited 0. Nightly rustfmt checked all 17 touched Rust files. Both WASM tools use `wasm-bindgen 0.2.127`.

The generated browser binding passes **374 bigint ABI checks** on each seed run. Differential seeds each match 275 cases: **1,100 cases total**. Forced divergence exits 1 with `OWNER_AUTH_DIFFERENTIAL_DIVERGENCE=DETECTED`.

Selected actual terminal output:

```text
     Summary [1138.562s] 2095 tests run: 2095 passed (2 slow), 41 skipped
     Summary [  93.794s] 489 tests run: 489 passed (2 slow), 3 skipped
test result: ok. 72 passed; 0 failed; 0 ignored; 0 filtered out; finished in 8.07s
ℹ tests 374
ℹ pass 374
ℹ fail 0
OWNER_AUTH_DIFFERENTIAL=PASS seed=38322398 fuzz_cases_per_fixture=24 corpus_cases=275
OWNER_AUTH_DIFFERENTIAL=PASS seed=1138 fuzz_cases_per_fixture=24 corpus_cases=275
OWNER_AUTH_DIFFERENTIAL=PASS seed=247 fuzz_cases_per_fixture=24 corpus_cases=275
OWNER_AUTH_DIFFERENTIAL=PASS seed=836 fuzz_cases_per_fixture=24 corpus_cases=275
OWNER_AUTH_DIFFERENTIAL_DIVERGENCE=DETECTED seed=38322398 count=1
```

The CLI 1,000-State publication and later-capture workload passed in **281.153 seconds**, within its unchanged six-minute timeout. The final gate sequence is serial; no build replaces metadata beneath a running test executable.

Complete gate inventory:

| # | Gate | Result |
| ---: | --- | --- |
| 1 | `fmt` | PASS |
| 2 | `affected-tests` | PASS |
| 3 | `affected-clippy` | PASS |
| 4 | `workspace-clippy` | PASS |
| 5 | `ci-feature-clippy` | PASS |
| 6 | `workspace-tests` | PASS |
| 7 | `cli-serialized-units` | PASS |
| 8 | `cli-ci-integration` | PASS |
| 9 | `cli-ci-suite` | PASS |
| 10 | `hosted-client` | PASS |
| 11 | `thread-wasm-core` | PASS |
| 12 | `thread-wasm-attachment` | PASS |
| 13 | `thread-dependency-boundaries` | PASS |
| 14 | `thread-default` | PASS |
| 15 | `thread-signing` | PASS |
| 16 | `thread-root-attachment` | PASS |
| 17 | `thread-root-clippy` | PASS |
| 18 | `thread-replication-check` | PASS |
| 19 | `thread-core` | PASS |
| 20 | `thread-native` | PASS |
| 21 | `thread-iroh-replication` | PASS |
| 22 | `semantic` | PASS |
| 23 | `thread-behavior` | PASS |
| 24 | `thread-behavior-clippy` | PASS |
| 25 | `thread-transport-observation` | PASS |
| 26 | `repo-thread-matrix` | PASS |
| 27 | `objects-writer-lease` | PASS |
| 28 | `thread-clippy` | PASS |
| 29 | `thread-core-clippy` | PASS |
| 30 | `thread-native-clippy` | PASS |
| 31 | `thread-signing-clippy` | PASS |
| 32 | `thread-replication-clippy` | PASS |
| 33 | `thread-iroh-clippy` | PASS |
| 34 | `thread-example` | PASS |
| 35 | `capability-wasm-build` | PASS |
| 36 | `biscuit-wasm-check` | PASS |
| 37 | `capability-wasm-tests` | PASS |
| 38 | `npm-binding` | PASS |
| 39 | `npm-pack` | PASS |
| 40 | `bigint-differential` | PASS |
| 41 | `forced-differential` | PASS |

Fixture regeneration ran the pinned API generators in an isolated directory, including deterministic writer/acceptor capability seeds, all HYBRID/native/foreign/writer/boundary/attachment corpora, and the ignored claimed-owner generator. The native-genesis conformance generator emitted the strict-inventory positive and negative cases used by both runtimes.
