# HYBRID Parts 5–7: alpha.33

The workspace and standalone conformance adapter pin API
tag `c9bd6ba26bc5e028e064b725419cfb702ccc559c`, version `=0.31.0-alpha.33`.
Biscuit verifier dependencies select the same pin through the workspace patch.
Crate/npm versions are unchanged; `SYNC_MANDATORY_GATE` remains off.
PR #1972 targets integration. Its final gate receipts and CI status are reported
on the PR against the tested commit; publication readiness requires every gate.

## Atomic publication admission

Commit validates its exact stored preparation and permits bounded scheduling;
it grants no genesis admission. `verify_publication_admission` requires a P1
and its exact branch-result P3, equal witnessed milliseconds and transaction ID,
with P1's admission order strictly before P3 and the same executor ID. Heddle
uses API `check_import_genesis_publication_pair` for these checks. Both must be inside the sole
signed delegation's exclusive window, with owner authority and revocations live
at their own authenticated observations. P3 verifies the exact operation and
cumulative manifest through the API helper. Receivers additionally compose the
whole history through API `verify_import_bundle_witnesses`; Heddle does not
implement another composition verifier. That helper selects the unique P3 consumed
by each progressive manifest, rejects duplicate/unused P3s and a second P1 for
one genesis, and pairs P1 with that exact consumed publication.

Renewal submission/preparation/RPC/validation, control availability and original
window refusals are removed. The state read is alpha.33's bounded writer-only
snapshot; retry uses its exact typed target and a fresh host operation UUID. All
35 status/retry-refusal pairs exercise the API agreement matrix.
Single-delegation converter pairing replaces successor/original pairing.
Advertised 24-hour windows and disjoint sequential/concurrent siblings are
covered by maintained API vectors and client tests. Both the signed duration and
host-advertised D are bounded by API `MAX_DELEGATION_WINDOW_SECONDS` (604800);
a shorter certificate cannot excuse an excessive advertisement. Known discovered OIDs stay
pinned; size estimates allocate signed per-job totals without per-branch budgets.

Commit maps `ALREADY_EXISTS` / `IMPORT_DESTINATION_CONFLICT` to
`HostedError::Hybrid(Reject::PreparationRefused(DestinationConflict))`, and
`ABORTED` / `VERSION_CONFLICT` to `HostedError::Hybrid(Reject::StaleContext)`.
Both fields must match; unrelated failures preserve structured detail. These
admission refusals have no accepted receipt and are not replay-frozen.

Deleted artifacts include the three retired consumer/control/review corpora,
`ImportRenewalSubmission`, the unused `receive_witnessed` API and its evidence
wrappers, and the entire `HostedImport` body/kind and legacy installer test.
The first adoption commit removes 2,742 Rust lines and adds 1,280 (net −1,462).
The job lifecycle loses 1,110 lines, receiver code 301, old receiver tests 424,
and the retired HostedImport file 526; 141 empty-base lines are retained separately.
The deterministic empty-base functions move to `thread_replication/initial_base`.
Weft integration/W2b have no `receive_witnessed` callers. Weft's old HostedImport
references belong to its separate hard-cut migration; no weft/API code is edited.

## Import discriminator and paths

`DelegatedImport` is opaque. Its construction authenticates the job signature
under verified delegation authority, exact native operation ID/content,
Thread/genesis/account, complete causal frontier and signed scope. A caller flag,
foreign operation or foreign frontier cannot construct this value. Native
signatures are verified independently before admission.

One branch result has one native tip Capture. Empty native frontiers preserve
the converted tip's ordered Git parents; ancestors travel as State closure.
The canonical synthetic seed is required and cannot be a Git parent. Nonempty
frontiers retain exact native source ancestry. Carrierless Captures and
LocalIntegration stay strict. Heddle creates no per-commit ancestor Captures:
`crates/ingest/src/state_writer.rs:73` writes ancestor States.

| Path | Carrier boundary or strict refusal |
| --- | --- |
| Bind/sign | `crates/object-model/src/object/thread_replication/delegated_import.rs:30` |
| Standalone parents | `crates/object-model/src/object/thread_replication.rs:352` |
| Native dual verification | `crates/crypto/src/import_authority.rs:124` |
| Witness-history closure | `crates/crypto/src/import_authority.rs:492` |
| Receiver install | `crates/repo/src/thread_replication/delegated_import.rs:690`, carrier binding at 825 and dual verification at 1094 |
| Replica admission | `crates/repo/src/thread_replication/mod.rs:1006`, exact bound value checked before `admit_ready` at 1120 |
| Current authority | `crates/capability-verifier/src/import_delegation.rs:325`, operation scope/budget at 489; native ancestry goes through dual verification |
| Hosted export and ingest | `crates/thread-api/src/replication/native/hosted.rs:135`, re-installs evidence before export; missing carrier refuses at 278 and 298 |
| Generic peer export/receive | `crates/thread-api/src/replication/native.rs:103` and 117 require hosted trust for imported evidence |
| Direct staging | `crates/thread-api/src/fetch/staging.rs:99`, exact operation binding/parents at 696 |
| Provider staging | `crates/thread-api/src/fetch/provider.rs:321`, same source validator |
| Publication staging | `crates/thread-api/src/publication/staging.rs:25`, exact enclosing bundle and same validator |
| Client source download | `crates/hosted-client/src/hosted_runtime/hosted/native_provider.rs:151`, independent root/owner and authenticated carriers |
| Native-only install | `crates/repo/src/thread_replication/native_witness.rs:230`, strict closure without an import carrier |

## Policy and effective owner intervals

`SelectedAuthority::policy_revocations` (`authority.rs:348`) treats exactly the
authenticated `(0, zero32)` head as empty revocations. A signed replacement
record refuses. Import/native predicates both use this rule; positive heads need
complete verified signed history, while absent/unknown observations fail closed.
The earlier bundle-level zero-record and unknown-head negatives also rejected
upstream, so they did not isolate this local guard. New tests call both predicates
DIRECTLY after injecting an owner-signed zero record into already verified owner
history; mutation proofs remove the actual local guard.

`import_owner_facts` uses replayed verified states, their activation times and
next accepted transition. Resource-transfer acceptance additionally caps the
previous root and starts the next owner at that exact time. Key, effective expiry
and interval all come from that historical state, including deferred claims.
Claim and transfer tests resolve the prior state before acceptance;
the API refuses a certificate claiming a different effective owner.

Staging now checks EACH operation's own publication observation, even when
operations share one delegation. A job-key revocation between publications
rejects staging just as it rejects installation. A genuine carrier with a signed
noncanonical genesis base now isolates the canonical-base check.

## Reproduction and proof

`python3 scripts/regenerate-hybrid-alpha33.py` archives the pinned API, runs its
maintained generators and frozen-vector continuity check, then regenerates
Heddle's imported-root positives and every dependent signature. The claimed-owner
fixture uses Heddle's maintained signed generator. No signature is hand-edited.
The tag continuity result is 272 changed, 65 added, 304 retired and 322
unchanged records, 54 identical files and three retired corpora. The tag import
corpus contains 150 signed and 233 wire records before Heddle root generation.

Affected tests use fresh `HEDDLE_HOME`. Guard proofs run on isolated committed
sources, require a named runtime assertion failure, restore bytes, and require
passing controls. Compile failures and empty test selections cannot count.
The maintained proof script covers imported tip roots/linear history/disjoint-root
merges; strict carrier/base/frontier rules; import/native genesis policy; direct
signed-zero-record predicates; claim and transfer owner intervals; each
publication's revocations; Commit-only/P3 refusal; live and atomic P1/P3 pairs;
executor equality; host window bounds; and typed Commit wire refusals. Part 2
and Part 4's guard families run at the same final commit. Receipts require
named runtime failures and unchanged passing controls, never compile failures.

The previous CI publication test sent a valid imported root through carrierless
staging. It now authenticates the enclosing bundle with
`authenticate_import_carriers` before calling
`validate_source_artifacts_with_import_carriers`, exactly as a production host
must. The carrierless entry point keeps its strict verdict. Receiver tests use
a live witness set and check typed pairing/window refusals, including duplicate
and unconsumed publications, executor mismatch and statement-array reordering.

The complete final gate includes nightly formatting, workspace and CI-feature
clippy, fresh-home workspace tests with serialized CLI units, CI CLI integration,
Thread API feature/dependency matrix, hosted-client tests, portable WASM builds
and tests, npm packaging, checked bigint boundaries, four differential seeds and
forced divergence, and the Part 2/4 guard-removal proofs. Exact commands, counts,
commit SHA and GitHub CI (including Windows) belong in the final PR receipt.

Applied surfaces: wire evidence, import/native content, durable receiver,
staging, replica export/ingest and WASM refusal/bigint bindings. No verb, flag,
human/JSON output or signing-format change. All ordinary and reverse rejection
paths remain strict, including the API's frozen old-parentless Capture negative.
