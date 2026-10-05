# HYBRID Part 5: alpha.32, scoped continuation

Both workspace API entries and the native conformance adapter select
`77f73219b72f02c6fcef21cf11e7c37ec2da2ff7`, the published alpha.32 tag,
with version `=0.31.0-alpha.32`. Crate/npm versions remain unchanged and
`SYNC_MANDATORY_GATE` remains off. PR #1972 targets integration and stays draft.

The owner scope change of 2026-10-05 defers alpha.33 adoption, full release
gates, the CI watch and READY. This continuation finishes the imported-tip rule,
the exact genesis-policy head for import/native revocations, and honest owner
intervals. Previously adapted alpha.32 lifecycle code is retained for the next
hard cut; this run does not extend renewal or scheduled admission behavior.

## Import discriminator and validation paths

`DelegatedImport` has private fields. `bind` verifies the API operation's job
signature under an authenticated delegation, the exact Thread/genesis/account,
complete causal frontier, resulting native operation ID and Capture commitment.
Only that bound value selects imported ancestry. It cannot be minted by a flag,
an unsigned kind, another operation's carrier or a carrier with another frontier.
Native original signatures and publisher job-key roles remain independently
verified. The signing formats, State IDs and shared Git converter do not change.

One branch slot has one native tip operation. With an empty native frontier,
Git ancestors remain ordered State-parent closure. No native ancestor Capture
is manufactured or demanded. The synthetic base must be canonical and must
never be a tip State parent; duplicates reject. Nonempty frontiers retain exact
native source ancestry. Ordinary Captures and LocalIntegration remain strict.

| Path | Boundary and verdict |
| --- | --- |
| Binding/signing | `crates/object-model/src/object/thread_replication/delegated_import.rs:30`: exact signed carrier before selecting import ancestry. |
| Standalone validation | `crates/object-model/src/object/thread_replication.rs:372`: strict, without a carrier. |
| Dual signature verification | `crates/crypto/src/import_authority.rs:125`: native signature, original converter job-key role, scope and carrier binding. |
| Witness/causal history | `crates/crypto/src/import_authority.rs:412`: exact opaque bound original; every other original stays strict. |
| Receiver installation | `crates/repo/src/thread_replication/delegated_import.rs:659`: independently selected API witness/owner/policy history; carrier-aware closure at line 790 and dual verification at line 1127. |
| Transactional admission | `crates/repo/src/thread_replication/mod.rs:1005`: bound original must equal the operation; `admit_ready` at line 1122 uses it only for that exact pending original. |
| Generic repository receive | `crates/repo/src/thread_replication/mod.rs:968`: witnessed import jobs require the hosted route; no carrierless exception. |
| Current/live authority | `crates/capability-verifier/src/import_delegation.rs:331` and line 440: permission/current cumulative-budget checks; native ancestry still requires the dual-binding boundary. |
| Hosted replica ingest | `crates/thread-api/src/replication/native/hosted.rs:296`: complete carried evidence re-enters receiver installation. |
| Hosted export/relay | `crates/thread-api/src/replication/native/hosted.rs:258`: refreshed proof and receiver re-admission at line 135; missing evidence refuses. |
| Generic peer export/receive | `crates/thread-api/src/replication/native.rs:87` and line 115: import evidence requires hosted trust; carrierless operations remain strict. |
| Direct source staging | `crates/thread-api/src/fetch/staging.rs:99`: authenticated certificates, then exact operation binding at line 697; ordinary staging stays strict. |
| Provider staging | `crates/thread-api/src/fetch/provider.rs:321`: same carrier and artifact/State-closure validator. |
| Publication staging | `crates/thread-api/src/publication/staging.rs:25`: exact enclosing proof bundle and same source validator; ordinary/owned-device publication stays strict. |
| Client source download | `crates/hosted-client/src/hosted_runtime/hosted/native_provider.rs:151`: independently pinned root/Spool owner, refreshed witness proofs, authenticated carriers before direct/provider staging. |
| Native-only witness install | `crates/repo/src/thread_replication/native_witness.rs:230`: no import carrier, strict ancestry; native evidence cannot grant the exception. |
| Single native witness receive | `crates/repo/src/thread_replication/delegated_import.rs:1651`: `NativeEvidence` has no import carrier and uses strict closure at line 1703. Its regression rejects unrelated imported tips even when already installed; the ordinary native control and exact dependencies still pass. |

No Heddle production path creates per-commit imported native ancestor Captures.
`crates/ingest/src/state_writer.rs:73` converts Git commits to State objects.
The preserved weft repro calls `sign_native_converted_in_tx` in
`crates/weft-hosted/src/server/hosted/integration_v2/import_job_authority.rs:574`
and `DelegatedImport::bind` through `crates/weft-storage/src/import_job_custody.rs:274`.
Those are one-slot native signing boundaries, not a reason to insert ancestor
operations. No weft file was modified in this run.

## Policy and owner history

`SelectedAuthority::policy_revocations` at
`crates/thread-api/src/hybrid/authority.rs:348` accepts precisely an authenticated
`(sequence 0, zero32)` head as empty revocations. A signed replacement record is
refused. Positive heads still require their complete authenticated signed chains;
unknown/absent observations fail closed. Both `import_revoked` at line 431 and
`native_revoked` at line 480 use the same rule and independently selected owner.
Fresh receiver positives cover import and ordinary native evidence. Negatives
cover a missing positive chain, unknown/absent heads, a signed zero-head record,
and a real policy revoking the job key.

`crates/repo/src/thread_replication/delegated_import.rs:418` constructs effective
owner facts from replayed, signature-verified accepted history, using each state's
activation floor and the next accepted state's floor. `owner_fact_at` at line 381
selects the interval containing the API-authenticated time. Key and effective
expiry come from that historical state, including deferral cleared by a claim.
The claim test returns the prior authority/expiry with `[0, 1100)` before the
claim, the claimed authority with `[1100, infinity)` afterward, and proves the
API rejects the claimed-state certificate against the earlier effective state.

## Reproduction and scoped verification

`python3 scripts/regenerate-hybrid-alpha32.py` reproduced the exact tagged
corpora in an isolated API archive, passed upstream vector continuity, then
regenerated Heddle's import-root positives and every dependent signature.
The API checkout and signing formats were not edited. Claimed-owner inputs
were regenerated through Heddle's signed fixture generator.

Every runtime test uses a fresh `HEDDLE_HOME`. The affected selections cover
crypto imports, capability imports, object-model replication, repository hosted
trust and owner intervals, Thread API policy/source staging/provider paths,
and hosted import/provider clients. The real source pack test proves one
parentless tip stages and installs State closure only after carrier authentication.
Clippy covers touched crates in default and client configurations with warnings
and dead code denied. Default hosted-client checks its library, since client-only
unit-test support is unused in a standalone default all-target invocation.

The eight new guard families in `scripts/prove-native-witness-guards.py` exercise
import tip positives, strict default, seed exclusion, nonempty causal ancestry,
exact carrier binding, import/native genesis policy, and honest owner intervals.
Each isolated mutation must fail its named runtime assertion, restore byte-exact
sources, then pass. Compilation failures and zero selected tests cannot count.
All eight pairs passed on code commit
`554b92fb9094b45e1c23e6babc8d197ecdccbe7e`. The final receipt commit changes
only documentation; its Rust, manifests, fixtures and proof scripts are identical.

Run the scoped proofs after committing:

```bash
python3 scripts/prove-native-witness-guards.py \
  import-tip import-tip-strict-default import-tip-seed \
  import-tip-causal-frontier import-tip-carrier-binding \
  genesis-import-policy genesis-native-policy owner-effective-interval \
  --output /tmp/heddle-part5-scoped-guards
```

Actual affected test output (each selection used a fresh `HEDDLE_HOME`):

```text
crypto import_authority: test result: ok. 32 passed; 0 failed
capability import_delegation: test result: ok. 20 passed; 0 failed; 1 ignored
object-model thread_replication: test result: ok. 37 passed; 0 failed
repo hosted_trust_tests: test result: ok. 29 passed; 0 failed; 1 ignored
repo owner_interval_tests: test result: ok. 1 passed; 0 failed
Thread API HYBRID: test result: ok. 43 passed; 0 failed
Thread API policy_tests (including added zero-record rejection): test result: ok. 5 passed; 0 failed
Thread API fetch::staging: test result: ok. 16 passed; 0 failed
Thread API fetch::provider: test result: ok. 4 passed; 0 failed
hosted import_source: test result: ok. 36 passed; 0 failed
hosted native_provider::tests: test result: ok. 2 passed; 0 failed
ALL 8 FAIL-THEN-PASS PAIRS VERIFIED
```

Nightly rustfmt and `git diff --check` passed. Default affected all-target clippy
passed for object-model, crypto, capability-verifier, repo and Thread API; default
hosted-client library clippy passed. The client configuration passed all targets
for all six touched crates. Both configurations deny warnings and dead code.
The later receiver/test correction also passed repo/Thread API all-target clippy.
The standalone source-transfer-only library build passed; its supplementary
warnings-denied clippy still reports pre-existing native-only `StagedSource`
fields unused without the native feature. No full feature matrix was run.

Local receipts: `/tmp/heddle-part5-affected-*.log`,
`/tmp/heddle-part5-clippy-*.log`, `/tmp/heddle-part5-regeneration-final.log`, and
`/tmp/heddle-part5-scoped-guards/results.json`. The two ignored tests are fixture
printing and the crash-test child entry point, not skipped owner regressions.

## Alpha.33 deletion/rework map

| Area | Heddle location | Next run |
| --- | --- | --- |
| Renew/recovery | `crates/hosted-client/src/hosted_runtime/hosted/import_source/job.rs:270`, line 306, line 333, line 480, line 663, line 805 | Delete renewal read/submission/preparation/RPC/validation and renewal fixtures/tests. |
| Public exports | `crates/hosted-client/src/hosted_runtime/hosted/import_source.rs:14` | Remove `ImportRenewalSubmission` exports. |
| Control availability | `crates/hosted-client/src/hosted_runtime/hosted/import_source/job.rs:120`, line 219, line 698 | Delete `require_control`, control-availability gates and their use to infer terminal state; use alpha.33's remaining typed outcomes. |
| Original window | `crates/capability-verifier/src/observed.rs:463`, line 527; `crates/hosted-client/src/hosted_runtime/hosted/import_source/job.rs:192` | Delete `HybridOriginalWindowEnded` mapping and renewal-only original-admission helpers. |
| Scheduled Commit admission | `crates/capability-verifier/src/import_delegation.rs:313` and its admission tests | Rework Part 1d for same-transaction branch P1/P3 publication admission; Commit alone grants no genesis admission. |
| Witness composition | `crates/repo/src/thread_replication/delegated_import.rs:523`, line 548, line 751 | Adopt the alpha.33 helper and single-delegation contract; preserve honest history/time selection and cumulative publication accounting. |
| Original converter relation | `crates/crypto/src/import_authority.rs:125`, `crates/repo/src/thread_replication/delegated_import.rs:1114` | Simplify renewal-era converter/delegation pairing to the one-delegation job. |
| Sibling preparation/window | `crates/hosted-client/src/hosted_runtime/hosted/import_source/job.rs:509`, line 608 | Audit sequential siblings and 24-hour advertised windows against alpha.33; regenerate signed vectors with its final tag. |

Applied surfaces: Git import/native content, durable receivers, source transfer,
replica export/ingest and independently authenticated wire evidence. No CLI verb,
flag, human/JSON output or signing-format change. Carrierless/revoked/missing
history paths retain rejection and transactional rollback. WASM/package adoption
from the earlier Part 5 commits is retained; full WASM parity and release gates
are deferred by the owner scope change.
