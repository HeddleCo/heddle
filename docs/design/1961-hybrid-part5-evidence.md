# HYBRID Parts 5–6: alpha.33

The workspace and standalone conformance adapter pin API
`9697e97538607ec922595468205dea6d99d3d358`, version `=0.31.0-alpha.33`.
Biscuit verifier dependencies select the same pin through the workspace patch.
Crate/npm versions are unchanged; `SYNC_MANDATORY_GATE` remains off.
PR #1972 targets integration and stays draft. The alpha.33 tag lookup returned
no tag; the full release gate and CI watch have not run.

## Atomic publication admission

Commit validates its exact stored preparation and permits bounded scheduling;
it grants no genesis admission. `verify_publication_admission` requires a P1
and its exact branch-result P3, equal witnessed milliseconds and transaction ID,
with P1's admission order strictly before P3. Both must be inside the sole
signed delegation's exclusive window, with owner authority and revocations live
at their own authenticated observations. P3 verifies the exact operation and
cumulative manifest through the API helper. Receivers additionally compose the
whole history through API `verify_import_bundle_witnesses`; Heddle does not
implement another composition verifier.

Renewal submission/preparation/RPC/validation, control availability and original
window refusals are removed. The state read is alpha.33's bounded writer-only
snapshot; retry uses its exact typed target and a fresh host operation UUID.
Single-delegation converter pairing replaces successor/original pairing.
Advertised 24-hour windows and disjoint sequential/concurrent siblings are
covered by maintained API vectors and client tests. Known discovered OIDs stay
pinned; size estimates allocate signed per-job totals without per-branch budgets.

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
The upstream continuity result was 272 changed, 51 added, 304 retired, 322
unchanged records, 54 identical files and three retired corpora.

Affected tests use fresh `HEDDLE_HOME`. Guard proofs run on isolated committed
sources, require a named runtime assertion failure, restore bytes, and require
passing controls. Compile failures and empty test selections cannot count.
All 16 fail-then-pass pairs passed on code revision
`9791386302fc2d3d36724651408e8c23a01ab2af`: the three imported-tip positives,
strict default, seed/frontier/carrier/base guards, import/native genesis policy,
direct signed-zero-record predicates, claim/transfer intervals, per-publication
revocation, missing P3/Commit-only admission, atomic transaction/time/order, and
P1 liveness. The final proof-script correction composes multiple mutations to
one file; the P1 liveness pair then removes both overlapping enforcement checks.
The remaining diff changes that controller, conformance inputs and receipts,
not the guarded production Rust. Receipts: `/tmp/heddle-alpha33-final-guards/results.json`
(15 pairs) and `/tmp/heddle-alpha33-window-guard/results.json` (one pair).

Actual affected outputs:

```text
crypto import_authority: 33 passed; frozen old-parentless/retired-kind: 1 passed
capability import_delegation: 23 passed; 1 ignored (fixture printer)
object-model thread_replication: 26 passed
repo owner_interval_tests: 2 passed
repo hosted_trust_tests: 26 passed; crash child rerun: 1 passed; 1 ignored child entry
Thread API hybrid: 47 passed
Thread API fetch: 29 passed; corrected hosted selection: 3 passed
hosted import_source: 28 passed
CLI import_cli_contract (ci): 11 passed
Biscuit verifier: 63 unit + 1 conformance passed; native-provider: 1 passed
WASM build: passed; checked bigint boundary: 374 passed
OWNER_AUTH_DIFFERENTIAL=PASS seed=38322398 fuzz_cases_per_fixture=24 corpus_cases=272
ALL 16 FAIL-THEN-PASS PAIRS VERIFIED (15 + 1 receipts)
```

The repository crash control initially lost its executable during a concurrent
build; its independent rerun passed. Fetch controls now authenticate carriers
before staging imported roots. No compilation failure is counted as proof.
Default and CI-feature clippy pass for all touched crates with warnings/dead
code denied; the standalone conformance adapter also passes. Nightly rustfmt
and diff checks pass. Logs: `/tmp/heddle-alpha33-*.log`.

The final tag lookup still returned no `v0.31.0-alpha.33`. Consequently the
workspace release gate, complete feature matrix, four-seed release parity,
Part 2/4 release proofs and CI watch have NOT run. PR #1972 remains draft.

Applied surfaces: wire evidence, import/native content, durable receiver,
staging, replica export/ingest and WASM refusal/bigint bindings. No verb, flag,
human/JSON output or signing-format change. All ordinary and reverse rejection
paths remain strict, including the API's frozen old-parentless Capture negative.
