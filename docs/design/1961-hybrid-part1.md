# HYBRID Part 1: core verification (#1961)

Implements the Heddle object-model, crypto, capability-verifier and repo rows of
[weft#2469's accepted design](https://github.com/HeddleCo/weft/blob/main/docs/design/2469-import-authority-and-host-witness.md).
The wire contract remains `heddle-api =0.31.0-alpha.21`, revision
`05c4d08c4e7120c2532a2b2a6c608b12e58acc4b`. Every expected conformance byte comes
verbatim from that tag's `import-authority-host-witness-v1.json`; the three leaf
packages retain their own identical copy for packaging. Malformed signed inputs
use the fixture seeds; tests never regenerate expected signatures.

Alpha.21 retains the alpha.20/api#319 binding, which selects the exact signed native acceptance, complete
originals manifest, publication intent and per-original receipts. The native
adapter verifies canonical formats, complete membership, original/account/Spool/
kind and receipt issuer/basis, then verifies accepting authority at the
root-authenticated witness observation with explicit original-subject scope.
Every acceptance in a dependency payload is verified. Publication and landing
retain OriginalAuthority. Native formats and signature domains are unchanged.

## Verification and durability

`capability_verifier::import_delegation` verifies independently selected immutable
Spool lineage and public owner/transfer history before owner → device import
permission → job attenuation. Current verification preserves actual receiver milliseconds, derives seconds
only for owner/delegation validity,
rechecks cancellation and owner/device/job revocations, and prohibits root,
witness, historical user-authority and cross-job key substitution. Selected root/
witness and permanently known job keys cannot become a delegator, device, owner
or genesis creator; the direct-owner delegation path remains supported. Fresh
receivers also exclude job declarations from the complete incoming bundle before
any mutation; only verified certificates become durable associations. Historical
verification takes accepted time/order only from an API-authenticated exact
witness; expiry or ordinary authorization revocation today cannot rewrite a
previous committed result. The complete signed ownership-transfer chain is
authenticated once, then every statement selects its exact verified historical
prefix and owner state. Current disclosure uses today's independently selected
authority. Cancellation IDs use the API's separate namespace.
Online roles, ordinary credentials, PURGE and timeline proofs supply no import
permission. Witness set/proof verification stays in API.

`object_model::thread_replication::delegated_import::DelegatedImport` binds the
verified API job operation to unchanged native Capture content, account-owned
genesis, complete expected/resulting frontier and causal State ancestry. Its
opaque value is content permission, never host publication or
`TrustedHostedExecutor`. Native converted records retain their original
converter signature across renewal; the successor API job signature commits to
those exact original bytes/IDs. The native converter must still be a verified
job key from that same logical job/retry lineage. The opaque
`VerifiedImportGenesis` retains original creation authority across later owner
transfers/renewals; current owner state never replaces original authorship.

`crypto::import_authority` independently verifies every signature/domain and
retains original native bytes, creator envelopes, typed permissions, job records,
first authority/ownership admission and landing request/source/reviews. Its
`NativeClosure` keeps genesis/creator/basis and landing ancestry checks, including
co-signed ownership claims and complete conflict resolutions. It validates each
bounded wire/native record; callers page transfer/closure depth. Landing retains
the exact user request proof and the unchanged executor frontier-CAS assertion;
it does not turn a multi-head target frontier into an invented single State.

`repo::thread_replication::hosted_trust` serializes fresh set authentication,
root epoch/generation/digest high-water, immutable seals/tombstones, receiver
clock floors and durable native changes in SQLite IMMEDIATE transactions. It
rechecks set expiry and wall/monotonic progress before commit. Independently opened
handles for the same canonical store/authority share a process-lifetime anchor;
dropping handles or reopening cannot clear a detected clock failure. SystemClock
uses one process-wide monotonic epoch. Custom clocks must share that epoch across
handles. Restored trustworthy wall time must catch up before mutations resume.
After signature verification, a final current-access callback receives freshly
sampled milliseconds under the same transaction immediately before commit.
The previous signed set is restored only from
locally retained, already authenticated bytes. Failed verification rolls back
both content and trust updates. Explicit routine root replacement preserves
local history and invalidates staged contexts; this is not automatic compromised
root recovery. No independently established provenance means history unavailable.

Schema 5 retains full public bundles, original signed witness sidecars, refreshable
retirement proofs, logical-job/slot commitments and permanent job associations.
A receiving bundle cannot enroll roots, replace genesis/envelopes/first statements,
or install another account as this device's owner. Installation leaves checkout
refs/files unchanged and publishes the normal post-commit change notification.
Bare direct/stored executor-pin routes now fail closed, including replay after
restart. Existing native admission/landing formats and signature domains remain
unchanged for structural verification.

## Applied surfaces

- **Verb:** existing Auth catalog and CI feature declarations now agree. This
  core change adds no CLI arguments; receiving still reports closed admission.
- **Human and agent:** typed Rust and WASM permission/evidence APIs expose exact
  rejection reasons for Part 2 output adapters.
- **Git interop:** original native bytes and Git authorship stay intact through
  existing sley publication controls.
- **Wire:** all evidence uses alpha.21 types; no second view RPC.
- **Reverse states:** root replacement, high-water/revocation inspection and
  unchanged bundle/admission export make durable trust state observable.

## Part 2 integration points

1. Independently select the canonical hosted authority and descriptor root, then
   call `hosted_trust::select_root`; select immutable Spool genesis/initial owner
   with `select_spool`. Never select either from the incoming set/bundle itself.
   Open `HostedTrust` with a trustworthy receiver clock and fetch a fresh complete
   signed set for each durable use, including exact replay.
2. Supply `delegated_import::AcceptedAuthority` from independently verified public
   owner/keyring histories and selected accepted state/order. Its revocation
   callbacks refer to the exact historical acceptance. `authorize_import` must
   check today's disclosure/audience/source access inside the transaction,
   including the final call with fresh receiver milliseconds. Do not use an
   earlier captured time.
   Current leases, sender epoch/policy/frontier fences and RPC credentials remain
   separate gates; this historical receiver API does not mint current permission.
3. Stage bounded native closure pages in the object store, then pass unchanged
   canonical bundle bytes and converter originals to
   `ThreadReplica::install_hybrid_import`. It verifies all carried receipts and
   portable dependencies and atomically installs genesis/content/proofs/slots.
   Renewals retain committed originals and narrow remaining scope under a fresh
   user signature/key; first genesis authority still uses its original parent.
4. For known-Thread native source/control/landing, use `receive_witnessed` with
   `NativeEvidence`, exact selected original policy and a current disclosure
   callback. Use crypto's typed authority/ownership/landing verifiers when wiring
   ownership carriers; retain their unchanged co-signed originals and native
   owner/conflict/frontier gates. A job grant is never one of these grants.
5. Export unchanged `hybrid_import_bundle` and `hosted_admission` sidecars. Refresh
   the set and obtain exact retirement inclusion proofs without replacing or
   re-signing original testimony. Re-resolve any cached opaque context at mutation.
6. `verify_current` / `verify_new_operation` are sender-side portable permission
   checks. Browser consumers can use WASM `verifyImportDelegation`; it does not
   resolve witnesses or grant transport access. Keep mandatory peer feature and
   disclosure checks in the Part 2 adapters. This PR keeps their 0.28.8 HYBRID
   rejects; it adds no thread-api/hosted-client production integration.
7. Carry alpha.21's exact boundary evidence in genesis/authority payloads and
   use the native adapter above. Every selected dependency acceptance needs its
   complete manifest/intent/receipt proof. Prepare/Commit RPC wiring belongs to
   Part 2 (api#322 / alpha.21).

## Acceptance table coverage

Each negative has a genuine passing fixture/control and checks its intended
rejection. The following are the Part 1 portions of the design table; host-only
operations are explicitly separated so core tests are not mistaken for deployment
or cross-replica proof.

| Design gate | Part 1 evidence | Remaining owner |
| --- | --- | --- |
| Root authentication / integrity | Wrong-root set, unchanged-signature tamper, independent root/Spool pins; stored endpoint pin cannot authorize | Part 2 descriptor transport attestation routing |
| Canonical / semantic set | Genuine root-signed duplicate/unsorted IDs, selector/purpose/interval/archive/window errors; noncanonical/oversized protobuf; immutable lifecycle transition/equivocation checks | API owns complete set semantics; no invented global interval restriction |
| Unattested witness | Genuine absent-key signature rejects with valid set; endpoint enrollment/real transport sidecar cannot grant witness authority | Full descriptor transport-only attestation exchange is Part 2 |
| Role substitution | Genuine device-signed owner/device/witness/root-as-job certificates; owner-signed witness/job-as-delegator permissions with distinct child jobs; historical witness-as-genesis-creator/publication negatives; direct-owner control in public JS/WASM | None in core |
| Owner / genesis binding | Selected owner/genesis/transfer chain, genuine A→B pre/post-transfer history, exact prefixes and fork/owner negatives on fresh/existing stores; fully recommitted wrong creator envelope; invalid carried owner root despite cached valid selection; no foreign-owner enrollment | Exact alpha.21 boundary adapter covered; transport integration is Part 2 |
| Delegation scope / authority | Genuine job-signed mutations of Spool/genesis/job/retry/delegation/ref/slot/hash/OID/target/frontier/options/converter/budget; signed source/provider/destination/mutable-ref/TTL/purpose attenuation and self/subdelegation negatives | Online prepare/commit adapters in Part 2/weft |
| Job key isolation / custody | Conflicting supplied and durable key→job associations reject atomically; independent signed operation/job binding | weft AEAD, rewrap, custody and plaintext destruction |
| Expiry / late work | Not-before/exclusive expiry and staged current checks; exact committed history survives ordinary expiry with authenticated witness time | Host recurring sync/publication fences |
| Renewal / idempotency | Complete published renewed export installs/replays exact original genesis, slots and signatures; immutable bundle originals; API renewal attenuation/cumulative manifest | Host lease/retry/publication accounting |
| Current authority races | Staged owner/device/job cancellation/revocation/expiry rechecks; current disclosure loss rejects historical install with unchanged control | weft current policy/transfer/lease/frontier final commit fence |
| Retirement / backdating | Exact archived receipt passes; genuinely backdated new receipt with old proof, missing proof and retired new work reject | Host issuance fence |
| Proof substitution / bounds | Published empty/single/even/odd trees plus purpose/executor/index/count/shape/missing/extra sibling and oversized proof negatives before install; full independent original authority retained | Retrospective proof lookup service |
| Revocation versus retirement | Identical original/proof accepted retired, rejected revoked; existing receipt/bundle/old pin and staged context cannot revive it | None in core |
| Rollback / restart | N+1 persisted, restart replays N; same-generation different body; receiver clock rollback/unavailable/expiry during transaction and independent handle/drop/reopen protection; fresh-receiver contract is not universal replay prevention | None in core |
| Cached contexts / concurrent use | Independent writer advances revocation; stale staged install rejected under shared SQLite serialization; root epoch replacement and disclosure-expiry-at-commit rollback | Part 2 context/network refresh and weft authority races |
| Root transition / compromise | Routine explicit replacement keeps known seals/tombstones; enlarged archive rejected across replacement, original local snapshot required | Operator-selected independent pre-compromise checkpoint/recovery workflow |
| Publication / landing independence | Exact operation/manifest/publication binding and original native source/request/review/ancestry; no job executor grant; native receive original control/revoked replay | Hosted landing policy/CAS execution in weft |
| Completeness / compatibility | Required owner/delegation/genesis/publication/retired proofs and native dependencies; strict decode; bare receipts cannot install | Part 2 mandatory peer negotiation, Fetch/export carrier integration |
| Issuance / retrospective lookup / destruction | No core claim of server journal fencing, lease ownership, proof lookup privacy or seed deletion | weft |

## Part 2 regression deferrals

The old `hosted_clone_writes` fixture tries to enroll its authenticated transport
key as an evergreen executor before every clone/write/discussion regression.
Closing that path makes all 38 default-feature cases fail during setup, before
their assertions run. Their bodies remain intact with explicit `heddle#1961
Part 2` ignores (three more cases are compiled only with `ci,preview`). Remove
those ignores when Fetch supplies complete fresh HYBRID evidence; they do not
claim a passing round trip in this Part 1 PR.

The active `transport_authenticated_fetch_cannot_enroll_an_evergreen_executor`
regression uses the same genuine source publication, signed HTTPS descriptor and
authenticated CLI clone. It verifies exit 76 at the closed path, an incomplete
clone, the byte-identical original signed genesis and no admitted source
operation. The adopted-history, batched-publication and four hosted-client
clone/pull regressions likewise preserve genuine publication/authorship controls
and require the receiver to fail closed. The hosted-client controls cover replay,
repair and hydration, unchanged local closure and unchanged unadmitted originals.
No Part 1 negative test is ignored.

Verification command output and guard-removal evidence are recorded in
[1961-hybrid-part1-evidence.md](1961-hybrid-part1-evidence.md).
