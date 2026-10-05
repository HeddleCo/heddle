# HYBRID Part 8: alpha.37 writer authority and mixed-origin closures

Both API entries select `=0.31.0-alpha.37`, git revision
`13052260f19fd94a06eaf5f1d06e2a00116b3c8f`. The registry consumers and standalone
conformance verifier select the same exact release. This is a hard cut; there is
no alternate wire reader or compatibility admission path.

## Receiver and authority changes

Foreign originals resolve inside the repository installation transaction from its
own durably installed journal. The resolver checks exact original bytes and
signature, immutable origin and Thread Genesis, deployment authority, Spool UUID
and Spool Genesis, the original P3 binding or native admission, and the exact
`prefix_admission_order`. It projects that prefix and runs the ordinary complete
installer verifier in read-only mode against the current witness set. Recursive
foreign obligations must already be installed. Cycles refuse; successful exact
tuples are cached only inside that transaction. Failed checks publish no artifacts
and commit no journal or trust changes.

Authority selection and prefix projection now live in `heddle-repo`, so recursive
receiver verification uses the same policy, owner-history, P1/P2/P3/P4, boundary,
causal, State, and LocalKey ownership checks as ordinary installation. Read-only
replay checks installed originals without advancing or rolling back retained
import high-water marks. `OriginalGeneses` resolves each immutable Genesis's own
envelope and origin.

Fetch installs foreign prefixes before their dependent carrier; each message has
one carrier. Prefix projection keeps exact causal ancestors and signed evidence.
Retention and export preserve each installed carrier's `foreign_dependencies`
verbatim; projection includes only the obligations consumed by that prefix.

Spool governance and writer authority resolve independently. Native Genesis uses
its immutable Account, P2 uses the signature-verified original actor, landing uses
the verified sealed-token subject and request key, and boundary acceptance uses
the signature-bound acceptor. The mandatory API actor-binding helpers run after
those native verifications. A co-writer's own verified history and attachment
inventory authorize the writer; its root cannot claim the Spool owner's UUID with
a different owner identity.

Landing dispatches source authority by source role. Account sources and Reviews
need their own P2 admission, LocalKey imports need their exact verified import
carrier and bound job publisher, native LocalKey work needs its admitted ownership
frontier as of the landing, and hosted integration sources need their earlier P4.
Source authority is checked at its own admission rather than renewed at landing.
P4 `review_evidence` contains native Reviews only. Known import jobs cannot sign a
landing request.

Grow-only `revoked_key_ids` cuts the actor publisher and mint root as well as claim
and resolution counterparties. Basis 2 cuts the verified acceptor; a revoked
original remains signed provenance. Hosts supply durable attachment enrollment;
receivers admit only exact witness-attested attachments. Retained issuers come
from verified owner history, survive Rotate, and refuse across Recover.

## Boundary prefix limitation

Per the orchestrator's explicit decision, a deterministic prefix refuses with
`Reject::Scope` when a complete signed boundary selection needs an original whose
P2/P4 sidecar lies beyond the prefix cutoff. There is no partial installation,
proof-only transport channel, or replacement of originals with receipts.

The retained repro has P1 admission order 201 and P2 order 204, selecting both a
Genesis and an Account source. The complete carrier passes full native and
transactional verification. Projection at the Genesis's P1 cutoff refuses with
typed Scope. A manually projected carrier passes the API's portable checks but
fails full native verification and leaves the receiver unchanged. Supplying the
exact omitted original makes the full-verification control pass, demonstrating
the missing provenance rather than a signature defect.

This known launch limitation is tracked upstream; cross-origin landing and prefix
staging are also deferred launch limitations in weft. The regression is
`boundary_p1_prefix_refuses_selection_beyond_cutoff` in
`crates/thread-api/src/hybrid/native_tests/retained_acceptor.rs`.

Both earlier upstream defects remain regressions here:

- alpha.35 cut a revoked original despite its valid boundary acceptor: the exact
  probe failed with `Revoked`; alpha.36 fixed it, and alpha.37 passes.
- alpha.36 extracted the original's attachment instead of the boundary acceptor's:
  the probe had 2 passing / 2 failing cases; alpha.37 has 4 passing / 0 failing,
  including rotated acceptor P1 and P2.

## Basis-2 audit

| Path | Verification retained |
| --- | --- |
| Native P1 | Exact original Genesis/binding/envelope, immutable Account, Spool lineage, complete acceptance selection and per-original receipt, current acceptor authority |
| Import P1 | Delegated Genesis binding/member provenance, complete boundary selection and current acceptor; basis 1 remains ordinary delegated admission |
| Native/import P2 source | Verified original actor before API binding, exact signatures/envelope/receipt, complete selection, signature-bound current acceptor |
| P2 claim/resolution | Both original signatures, immutable LocalKey Genesis, exact frontier/claim conflicts, signed accepting actor, subject-specific permission |
| Dependency acceptances | Each exact acceptance verified independently; complete selection and receipts; omission, exchange, duplicates and extras refuse |
| Revocation | Publisher/mint and credential identities selected from each signed acceptor; per-statement verified authority/credential cache; credentials attested at witness observation |
| Attachments | Alpha.37 signature-bound extractor, authenticated admission, verified issuer history, exact inventory membership, Rotate and Recover rules |
| Native bytes | Canonical acceptance/manifest/intent/receipts and original signatures; strict integer arrays for 32-byte native fields in Rust and TypeScript |
| Journal/replay | Exact installed bytes/origin/deployment, immutable Spool Genesis, fresh witness recheck; retained history is never a substitute for originals |
| Foreign continuation | Full ordinary prefix verifier in read-only mode, recursive installed obligations, exact cutoff and original causal/State/ownership checks |
| Projection/export | Complete selection required within the cutoff or typed Scope; signed evidence and consumed foreign references preserved |
| P3/P4 | Basis 2 is limited to P1/P2; P3 remains delegated publication and P4 original landing authority |
| Browser/WASM | Independent author history and authenticated attachment inventory are explicit inputs to Native Genesis verification |

The dedicated rotated boundary receiver positives cover native P1 and P2.
Import P1 and ownership claim/resolution use the shared boundary verifier and the
published import selection/substitution corpus; there is no new rotated import
P1 or rotated basis-2 resolution fixture and no live weft deployment claim.

## #1978

Forged-original tests now recompute payload, signatures and authority commitments
and genuinely re-sign the witness statement, so the expected refusal is native
`Signature`. Acceptor key cuts first prove the unrevoked identity resolves.
Verified authorities and credential identities are cached per statement.
Credential checks use authenticated witness testimony rather than pretending
credential IDs belong to Spool `revoked_key_ids`.

## Verification

Final gate results and guard-removal results are recorded below.
Every runtime command uses a fresh `HEDDLE_HOME`, `TMPDIR=/home/scratch` and
`CARGO_TARGET_DIR=/runner/heddleco-build/scratch/heddle-part8-target`. Only touched
Rust files are formatted with `rustfmt +nightly --edition 2024`.

Affected runtime output (fresh home, exit 0):

```text
heddle-crypto: test result: ok. 74 passed; 0 failed; 0 ignored
heddle-repo: test result: ok. 964 passed; 0 failed; 5 ignored
heddle-thread-api: test result: ok. 206 passed; 0 failed; 0 ignored
heddleco-capability-verifier: test result: ok. 110 passed; 0 failed; 6 ignored
```

Nightly formatting passes on all 34 touched Rust files. A workspace-wide check
also identified import-order drift in ten untouched baseline files; all ten were
verified byte-for-byte equal to HEAD. They remain untouched per the explicit
formatting scope. The final format gate is the requested touched-file check.

The full gate exposed timing-sensitive hosted fixtures: a 3-second credential
and 1.8-second witness set could expire before their valid controls reached the
intended check under parallel load. Setup now has 60 and 30 seconds respectively;
the tests still cross the actual expiry, require the final artifact hook, and
assert complete rollback. The hosted-client gate subsequently passed all 488
selected tests, including those controls.

Installation also reuses operations already authenticated by `NativeClosure`
instead of repeating their signature checks during role selection and install.
Canonical bytes, original signatures, reference validation, owner checks, and
transaction freshness remain verified. The 1,000-State/later-capture test passed
in 258.82 seconds after this change, compared with 314.85 seconds in the diagnostic
control. The CI timeout remains six minutes.

All **41/41** final gates pass. The required Rust commands executed **10,176 passing tests, 0 failing tests, and 87 existing ignores/skips**. These are execution counts across overlapping feature configurations, not unique test counts.

| Gate | Passed | Ignored / skipped |
| --- | ---: | ---: |
| `affected-tests` | 1,354 | 11 |
| `workspace-tests` | 4,604 | 25 |
| `cli-serialized-units` | 504 | 0 |
| `cli-ci-integration` | 31 | 0 |
| `cli-ci-suite` | 2,094 | 41 |
| `hosted-client` | 488 | 3 |
| `thread-default` | 245 | 1 |
| `thread-signing` | 20 | 0 |
| `thread-root-attachment` | 4 | 0 |
| `thread-core` | 16 | 0 |
| `thread-native` | 197 | 0 |
| `thread-iroh-replication` | 107 | 1 |
| `semantic` | 336 | 3 |
| `thread-behavior` | 2 | 0 |
| `thread-transport-observation` | 1 | 0 |
| `repo-thread-matrix` | 92 | 2 |
| `objects-writer-lease` | 9 | 0 |
| `capability-wasm-tests` | 72 | 0 |

Formatting, affected/workspace/CI-feature clippy, every Thread API feature clippy/check, both WASM portability builds, dependency boundaries, the runnable Thread example, release browser binding, and npm pack dry-run all exit 0. The WASM runner and binding generator are both `wasm-bindgen 0.2.127`.

The public generated browser package passes **374 bigint ABI checks**. Differential seeds `38322398`, `1138`, `247`, and `836` each pass **272 cases**, **1,088 total**. Forced divergence exits 1 and prints `OWNER_AUTH_DIFFERENTIAL_DIVERGENCE=DETECTED`, proving the comparison gate rejects a mismatch.

Selected terminal output:

```text
workspace: test result: ok. 4604 passed; 0 failed; 25 ignored
serialized CLI units: 504 passed; 0 failed; 0 ignored
CI integration: test result: ok. 31 passed; 0 failed; 0 ignored
CLI nextest: 2094 tests run: 2094 passed (3 slow), 41 skipped
hosted-client nextest: 488 tests run: 488 passed (2 slow), 3 skipped
WASM: test result: ok. 72 passed; 0 failed; 0 ignored
bigint: tests 374; pass 374; fail 0
OWNER_AUTH_DIFFERENTIAL=PASS seed=38322398 fuzz_cases_per_fixture=24 corpus_cases=272
OWNER_AUTH_DIFFERENTIAL=PASS seed=1138 fuzz_cases_per_fixture=24 corpus_cases=272
OWNER_AUTH_DIFFERENTIAL=PASS seed=247 fuzz_cases_per_fixture=24 corpus_cases=272
OWNER_AUTH_DIFFERENTIAL=PASS seed=836 fuzz_cases_per_fixture=24 corpus_cases=272
OWNER_AUTH_DIFFERENTIAL_DIVERGENCE=DETECTED seed=38322398 count=1
```

The final CLI gate includes the 1,000-State/later-capture workload: **308.603 seconds, PASS**, within the unchanged six-minute timeout. The final workspace gate, including doctests, ran serially after the feature matrix to avoid shared-target metadata replacement.

17 guard-removal pairs completed. Each named runtime assertion fails with exit 101 when its guard is disabled, then passes with exit 0 after exact source restoration. No compiler failure or empty test selection counts as proof. API mutations used an isolated alpha.37 copy; the final workspace retains the published git pin.

| Guard | Regression | Removed / restored |
| --- | --- | --- |
| `governance-is-not-author` | `cowriter_start_capture_own_and_owner_thread_and_review` | 101 / 0 |
| `foreign-resolver` | `foreign_import_tip_lands_into_native_fast_forward_and_merge` | 101 / 0 |
| `installed-status` | `foreign_original_requires_a_durably_installed_operation` | 101 / 0 |
| `attachment-required` | `paired_cowriter_after_rotate_uses_witness_admitted_attachment` | 101 / 0 |
| `attachment-exact-inventory` | `forged_old_owner_certificate_cannot_join_durable_attachment_inventory` | 101 / 0 |
| `actor-publisher-cut` | `receiver_policy_cuts_the_independent_actor_publisher_and_mint` | 101 / 0 |
| `actor-mint-cut` | `receiver_policy_cuts_the_independent_actor_publisher_and_mint` | 101 / 0 |
| `boundary-prefix-scope` | `boundary_p1_prefix_refuses_selection_beyond_cutoff` | 101 / 0 |
| `p4-own-admission` | `native_authority_ownership_and_landing_preserve_original_closure` | 101 / 0 |
| `root-owner-identity` | `writer_account_and_owner_identity_negatives_reject_then_controls_pass` | 101 / 0 |
| `requester-token-binding` | `landing_requester_must_be_the_verified_token_subject` | 101 / 0 |
| `p2-actor-binding` | `p2_envelope_cannot_replace_the_verified_operation_author` | 101 / 0 |
| `recover-cuts-attachments` | `paired_cowriter_after_rotate_uses_witness_admitted_attachment` | 101 / 0 |
| `writer-key-policy` | `actor_key_policy_cut_and_non_review_p4_reject_then_controls_pass` | 101 / 0 |
| `review-only-p4` | `actor_key_policy_cut_and_non_review_p4_reject_then_controls_pass` | 101 / 0 |
| `job-request-role` | `job_key_cannot_sign_landing_request` | 101 / 0 |
| `counterparty-policy` | `actor_key_policy_cut_and_non_review_p4_reject_then_controls_pass` | 101 / 0 |

Negative/control coverage additionally proves missing-stage refusal inside the installation transaction with unchanged durable table values and no artifact publication. The source and Review own-admission test genuinely signs their P2 evidence before testing the same P4. The two retained upstream probes have the original fail-then-pass version receipts described above.


## Coordinated weft API changes

Weft and browser consumers must adapt these exact names:

- `ForeignDependencyV1` and `ForeignDependencyOrigin`; the reference includes
  `format_version`, `origin`, `thread_genesis_digest`, `signed_native_digest`,
  `prefix_admission_order`. Both `ImportPublicProofBundleV1.foreign_dependencies`
  and `NativePublicProofBundleV1.foreign_dependencies` must be retained/exported.
- `verify_landing_key_roles`,
  `ImportBundleOwnerExpectation.forbidden_landing_keys`,
  `VerifiedWitnessSet::known_job_keys`, `local_work_cutoff`, and the fourth role
  argument of `native_witness::verify_bundle_witnesses`.
- `writer_authority::{verify_account_binding, verify_authority_actor_binding,
  verify_landing_actor_binding, WriterWitnessPayload, retained_mint_root_issuer,
  admitted_owner_mint_root_attachment, verify_retained_writer_attachment}`.
  Basis 2 selects the signature-bound acceptor for current writer cuts and
  attachment extraction; basis 1 cuts the ordinary actor and counterparties.
- `HostedLandingWitnessV1.review_evidence`: native Review operations only.
- Heddle `NativeAuthorityContext.author_authority` and
  `crypto::writer_authority::{AuthorAuthority, AuthorState, HostAuthorAuthority,
  WitnessedAuthors}`: independent actor history and authenticated inventory.
- `OriginalGeneses::{import,native,insert}`: per-Genesis origin/envelope selection.
- `native_genesis::AuthorAuthority` and `verify_original_authority`: explicit
  verified actor authority and retained inventory; `native_genesis::verify_bytes`
  and WASM `verify_native_genesis_authority_binding` add `author_history` and
  `admitted_mint_roots_json`.
- `thread_control_authority::{inspect_landing_subject, LandingSubject,
  verify_landing_request_with_retained_mint_roots}`: resolve verified token subject
  before requester authorization.
- `crypto::import_authority::{verify_authority_payload, verify_landing_payload}`
  take mutable `NativeClosure`, recording successful original admissions before
  verifying dependent landings. Verify source/Review P2 and earlier P4 evidence
  in admission order.

Weft's `require_owner_actor_in_tx` and its owner-only call sites must be removed
per QA7 option B, preserving role, authorization epoch and transactional witness
issuance checks. Hosts must pass their durable device attachment inventory;
receivers use witnessed attachments, not incoming certificates as enrollment.

Applied product surfaces: native/import receiver, wire, fetch/export, retained
history, reverse/refusal states, Rust/browser verification. There are no new CLI
verbs, flags, help or output changes, and no Git projection format change.
