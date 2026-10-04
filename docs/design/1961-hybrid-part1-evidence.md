# HYBRID Part 1 security-review evidence (#1961 / PR #1964)

The branch plain-merged `origin/integration` at `7f90c4ff` (merge `6fc36351`),
then moved the coordinated Rust/npm release to 0.28.8 for its dependency change.
Final API pin: `=0.31.0-alpha.21`, exact release commit
`05c4d08c4e7120c2532a2b2a6c608b12e58acc4b`. Alpha.21 published during this work.
It retains alpha.20/api#319's exact boundary binding and completed signing layouts;
Prepare/Commit integration (api#322) belongs to Part 2.

## Conformance source

All three packaged fixture copies are verbatim from the API release:

```text
30852e349b9b17ec7ea65df18d307be760eb908f04d4721ee7dc46ca4832ded2
```

The copies live in capability-verifier/conformance/hybrid, crypto/tests/fixtures
and repo/tests/fixtures/hybrid. All **64 alpha.20 signed vectors are unchanged**
in alpha.21. The earlier alpha.20 fixture hash was
`e7fd977bc39d5fc4937c55e29d1be00247859ef9413c152be55c3ab8522b8dfc`.
Expected signatures/bytes were never regenerated. Derived malformed regression
inputs use the fixture seeds and recompute every affected signature/commitment.
The review and its probes remain read-only under
`/home/scratch/review-heddle1964/`.

## Finding-by-finding resolution

| Finding | Resolution and discriminating control |
| --- | --- |
| P1 #1: disclosure expiry during verification | `HostedTrust::mutate_validated` checks current access with freshly sampled milliseconds under SQLite IMMEDIATE immediately before commit. The genuine complete export succeeds at 1350000, rejects initially at 1352000, and rejects when time advances past deadline 1351000 during verification. Threads, admissions, proofs, slots, job associations and signed-set updates roll back. Native receiving uses the same final hook. |
| P1 #2: alpha.18 incompatibility / missing boundary binding | Coordinated API and native-conformance pins, verbatim new fixtures and real native boundary verification. API selectors bind acceptance/manifest/intent/receipts; the native adapter checks full canonical selection, original/account/Spool/kind, issuer/basis, signatures and accepting authority at authenticated observation with explicit original scope. All dependency acceptances are checked. Publication/landing retain OriginalAuthority. Current and retired multi-original/multiple-acceptance controls pass. Five API substitutions and nine native selection negatives reject at their own gates. |
| P2 #3: ownership-transfer history | Authenticate the full signed chain once, then require each statement's exact verified prefix, historical owner/state and transfer sequence. Genuine A→B pre-transfer originals and post-transfer renewal retain original genesis and signatures on fresh/existing stores. Wrong prefix, wrong owner/final historical selection and a genuinely co-signed fork reject without durable changes. Current access remains independently selected. |
| P2 #4: witness/job delegating devices | Exclude selected descriptor/witness and permanently known job keys from delegator, user authority and genesis creator positions in current/historical paths, including native original and boundary acceptor authority. Genuine owner-signed permissions naming witness/job keys with distinct child jobs reject. The complete incoming bundle's job declarations also restrict roles on fresh receivers before durable associations exist; only verified certificates are persisted. A genuine B-signed post-transfer permission cannot promote the earlier job to delegator; ordinary device and direct-owner controls pass. Historical publication/genesis controls retain valid native commitments and genuine selected witness signatures. |
| P2 #5: reopen erases clock failure | Independent handles share a retained process-lifetime anchor keyed by canonical store/authority; SystemClock shares one monotonic epoch. Frozen wall time after 600000 ms rejects through independent handles and drop/reopen. Restored trustworthy wall time with a fresh genuine root-signed set succeeds. SQLite and restart wall floors remain. |
| P2 #6: coverage / public WASM route | The differential invokes public `verifyImportDelegation` with identical byte/JSON/time inputs and compares digests/rejection reasons. It includes signed wrong-role delegators, direct-owner/device controls, expiry, cancellation, all three key revocations, independent selection, missing parent, width and i64 overflow. Forced divergence targets import specifically. The genesis-envelope negative recomputes its binding, delegation and witness commitments before reaching envelope rejection. Subdelegation uses distinct keys; published empty/single/even/odd trees and native boundary vectors are consumed directly. |
| P3 #7: millisecond truncation | Preserve receiver milliseconds for witness freshness; derive seconds only for owner/delegation validity. A root-signed set issued at 1350001 and receiver time 1350002 passes both historical publication and genesis. No signed expiry grace is added. |

## Fail-then-pass runs

Mutations were temporary and restored before final gates. API guard mutations
used isolated release copies under `/tmp`; shared Cargo cache sources were never
edited. Logs are retained in `/home/scratch/heddle1964-fixes/`; final-graph gates
are in its `alpha21/` directory.

| Gate | Observed failing run | Restored regression |
| --- | --- | --- |
| Current disclosure / reopen clock | Review-seeded expiry and independent-reopen tests both fail against the original behavior (`regressions-before.log`) | `hosted_trust_tests` |
| Actual boundary support | Published positive fails with `BoundaryAcceptancePendingApi318` (`boundary-before-rerun.log`) | `review_alpha20_boundary_vectors_resolve_exact_originals` plus native complete controls |
| Exact API boundary binding | Isolated selector/original/manifest/intent/receipt checks removed: **0 passed, 5 failed**, each negative becomes `Ok(())` (`boundary-api-before.log`) | Five `review_boundary_api_*` tests |
| Complete native selection | Adapter selection/authority gate bypassed: **0 passed, 9 failed** (`boundary-native-before.log`) | Nine individually named `review_boundary_native_boundary_*` tests, current and retired |
| Mixed historical transfer prefixes | Old full-vector equality restored: **0 passed, 1 failed**, genuine mixed-history control rejects `Root` (`transfer-before.log`) | `review_transfer_preserves_original_genesis_and_exact_historical_prefixes` |
| Wrong-role delegator | Original role behavior: **0 passed, 1 failed**, genuine owner-signed witness delegation accepts (`roles-before.log`) | `review_wrong_role_delegators_have_genuine_owner_permissions` |
| Fresh receiver job promotion | Genuine A→B bundle with B signing a permission for the earlier job: **0 passed, 1 failed**, bad role installs (`alpha21/fresh-job-role-before.log`) | `review_fresh_bundle_job_cannot_become_post_transfer_delegator` |
| Operation scope | Operation scope verification removed: **0 passed, 1 failed**, signed scope violation becomes `Ok(())` (`scope-before.log`) | `portable_import_permission_scope_and_current_expiry` |
| Exact creator envelope | Envelope equality removed with all surrounding commitments valid: **0 passed, 1 failed** (`genesis-envelope-before.log`) | `genesis_original_owner_and_exact_envelope_remain_mandatory` |
| Retired history proof | Exact inclusion guard removed in isolated API: **0 passed, 1 failed** (`retirement-before.log`) | `retirement_backdating_requires_the_exact_sealed_statement` |
| Revocation versus retirement | Revoked-entry rejection removed in isolated API: **0 passed, 1 failed** (`revocation-before.log`) | `revocation_vs_retirement_rejects_identical_original_and_cached_context` |
| Public import differential | Forced mismatch in the import route must exit nonzero and identify `import-device-control` | Four restored fixed seeds, public JS binding |

The fail-then-pass boundary runs used alpha.20's unchanged fixed vectors. Final
alpha.21 tests rerun the same gates from its verbatim release fixture. Upstream
Prepare/Commit vectors are packaged intact; this evidence makes no Part 2 RPC or
host publication claim.

## Final gate output

Observed completed checks on the final alpha.21 graph:

- Nightly fmt on 25 touched Rust files and `git diff --check` passed.
- `cargo clippy --workspace --all-targets --locked -- -D warnings -D dead-code` passed.
- CLI `ci`, CLI/config `telemetry`, and thread-api default/core/native/signing/
  replication/iroh+replication/root-attachment/semantic-analysis Clippy passed
  with `-D warnings`, using CI's target selections.
- `cargo build --locked -p heddleco-capability-verifier --target wasm32-unknown-unknown --lib` passed.
- Ordinary verifier WASM check and portable thread-api core/root-attachment WASM checks passed.
- Full capability-verifier WASM unit suite: **48 passed, 0 failed**. All eight
  import-delegation tests actually executed, including roles and fractional time.
- Optimized npm build and prepack/dry-run passed with wasm-pack 0.13.1,
  wasm-bindgen 0.2.127 and wasm-opt 117. Package: `heddleco-capability-verifier-wasm-0.28.8.tgz` (10 files).
- Native conformance verifier built with `--locked` against the same alpha.21 pin.
- All four checked-in differential seeds passed: **139 cases each, 556 total**.
  Forced divergence exited nonzero with `OWNER_AUTH_DIFFERENTIAL_DIVERGENCE=DETECTED`
  and named `import-device-control` (fuzz count 0).
- Leaf dependency boundaries and publish inventory/dependency order passed
  (156 requirement/version pairs; 35 publishable crates).

Targeted final regression output:

```text
crypto import_authority_tests:
test result: ok. 26 passed; 0 failed; 0 ignored; 0 measured; 39 filtered out; finished in 23.63s
capability-verifier import_delegation:
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 71 filtered out; finished in 0.94s
repo hosted_trust_tests:
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 926 filtered out; finished in 14.58s
capability-verifier full WASM suite:
test result: ok. 48 passed; 0 failed; 0 ignored; 0 filtered out; finished in 5.49s
OWNER_AUTH_DIFFERENTIAL=PASS seed=38322398 fuzz_cases_per_fixture=24 corpus_cases=139
OWNER_AUTH_DIFFERENTIAL=PASS seed=1138 fuzz_cases_per_fixture=24 corpus_cases=139
OWNER_AUTH_DIFFERENTIAL=PASS seed=247 fuzz_cases_per_fixture=24 corpus_cases=139
OWNER_AUTH_DIFFERENTIAL=PASS seed=836 fuzz_cases_per_fixture=24 corpus_cases=139
```

The full workspace command completed with **exit 0: 6987 passed, 0 failed,
117 ignored**, summed across 176 unit/integration/doctest summaries
(2328.67 seconds, including build-lock waits and compilation). It used
fresh `HEDDLE_HOME=/home/scratch/heddle1964-workspace-tests-hkii2js4`, `TMPDIR=/home/scratch`,
`CARGO_TARGET_DIR` removed, and the worktree's isolated Cargo target unchanged:

```sh
cargo test --workspace --locked -- --test-threads 8
```

Recorder output:

```text
workspace-tests 0
```

The aggregate includes the explicit Part 2 and existing opt-in/platform ignores;
the targeted Part 1 regressions and all 48 WASM cases have zero ignores.

Full-graph core library output tails:

```text
crypto:
test result: ok. 65 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 16.82s
heddle_object_model:
test result: ok. 375 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.56s
heddleco_capability_verifier:
test result: ok. 75 passed; 0 failed; 4 ignored; 0 measured; 0 filtered out; finished in 4.39s
repo:
test result: ok. 990 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out; finished in 234.99s
hosted_client:
test result: ok. 447 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out; finished in 214.32s
heddle_thread_api:
test result: ok. 125 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 62.73s
```

## Integration limits

The 38 default `hosted_clone_writes` cases and three feature-specific native
carrier deferrals remain explicit Part 2 work. Bare direct/stored executor-pin
routes reject with typed `WitnessEvidenceRequired`; these rejection tests do
not establish a HYBRID Fetch/clone round trip. Descriptor routing, peer
negotiation, Prepare/Commit, custody, issuance fencing, retrospective lookup,
server policy/lease/frontier commit fences and compromise recovery remain with
Part 2/weft. No Part 1 negative is ignored.

Applied surfaces: no new verb/help/clap flag; human and agent output contracts
remain unchanged; the additive Rust/WASM evidence APIs carry typed rejections;
Git projection retains original signed bytes/IDs through sley; the shared API
wire is coordinated without another view RPC; transfer/replay/revocation/root
replacement and exported trust/proof state remain observable.
