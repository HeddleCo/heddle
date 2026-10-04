# Resume verification after Part 1 landed

Part 1 `f9056e6d` was plain-merged at `60eaabdd`. The resumed client uses API
alpha.21 and its original fixed vectors. The boundary acceptance transport
control passes with exact API binding; its missing-binding negative rejects.

The real durable installer interleaving proof is complete:

```text
# independent handle persists N+1; high-water guard removed in an isolated source copy
staged N must never authorize install after durable N+1 revocation
test result: FAILED. 0 passed; 1 failed; 139 filtered out
# same guard restored, same isolated test
 test result: ok. 1 passed; 0 failed; 139 filtered out
# resumed production HYBRID suite, including boundary and durable interleaving
 test result: ok. 15 passed; 0 failed; 125 filtered out
# prepared jobs, original signed scope, ordered branch limits, import/retry completeness
 test result: ok. 13 passed; 0 failed; 444 filtered out
```

Logs: `/tmp/hybrid-p2-cached-red.log`, `/tmp/hybrid-p2-cached-green.log`,
`/tmp/hybrid-p2-hybrid-restored.log`, `/tmp/hybrid-p2-jobs.log`. The original
Part 1 core files are unchanged. A first production-path run reused a temporary
copy's guard-removed Cargo artifact; it is not claimed as a passing run. Rebuilding
the restored core and rerunning passed. No cargo clean was used.

The old full-gate results below belong to the earlier alpha.19 checkpoint,
**not** this resumed head. The atomic filesystem callback, read-only snapshot and
selected native closure remain absent from landed Part 1; see
[hybrid-part2-needs.md](hybrid-part2-needs.md). They still block complete hosted
installation/relay and draft removal. Alpha.23 remains unpublished.

# Earlier HYBRID Part 2 verification evidence

Branch: `task/1961-hybrid-part2-transport-client`.
PR: https://github.com/HeddleCo/heddle/pull/1963.
The full workspace gate checked code/test tree
`877a01cc874f8d83174d563245ac700fb9c18bd2`. Production behavior is unchanged
from `c880c59dee0d791674b1210f8419c93f886da442`; subsequent changes correct
device RPC test assertions, isolate the bearer-retention test home, and apply
nightly rustfmt. The final evidence/corpus update is checked separately.
All commands use the configured isolated target:

```sh
export CARGO_TARGET_DIR=/runner/heddleco-build/scratch/heddle-1961p2-target
export TMPDIR=/home/scratch
export HEDDLE_HOME=$(mktemp -d /home/scratch/hybrid-p2-home.XXXXXX)
```

Integration's 0.28.7 release was plain-merged. Its API alpha.19 pin does not
change the alpha.18 HYBRID schema, documentation, verifier or fixed vectors.
The retained fixture contains selected published alpha.18 vectors. Every
retained value was compared to that published fixture and matches exactly;
expected signatures were not regenerated. Sync's mandatory gate remains OFF at the shared
`hybrid::SYNC_MANDATORY_GATE` switch. api#318 boundary acceptance remains closed.

## Transport negatives and passing controls

| Required row | Running negative and control | Evidence |
| --- | --- | --- |
| Completeness/protocol gate | `discovery_retains_peer_support_and_rejects_old_peers_before_the_gated_call`; complete fixed bundle versus missing closure; capable peer executes gated call, old/malformed peer never executes it | Portable library: 15 passed |
| Verify before install; no partial mutation | `verify_before_install_rejects_without_partial_repository_mutation`: genuinely signed foreign publisher stages structurally, then installation rejects before pack, Spool ID, owner pin or replica writes; selected local owner installs the same source | Native/default matrix passed; guard-removal experiment below |
| Structural staging never authorizes | `structural_staging_never_authorizes_an_account_genesis`; `structural_authority_sidecars_never_authorize_native_receive`: authentic structural originals/sidecars reject at admission, while a genuinely owned local original is admitted | Native/default matrix passed; authenticated device RPC control: 1 passed in 131.20s |
| Retrospective proof-only retrieval | `retrospective_lookup_returns_only_the_exact_originals_retirement_proof`; request is exactly the published 68-byte public selector, without credentials/resource identity; malformed selectors never reach lookup and missing originals return the same `NotFound` | Portable library: 15 passed |
| Proof substitution | `proof_substitution_cannot_authorize_a_neighboring_original`: another authentic leaf's proof rejects; exact genesis proof succeeds | Portable library: 15 passed |
| Cached contexts | `learned_revocation_and_changed_original_invalidate_a_resolved_context`: original context succeeds unchanged, then N+1 revocation/changed original rejects | API context control passed; **not** durable concurrent-install proof |
| Bundle preservation in relay | Negotiated genuine fixed-vector native original carries its byte-identical complete public bundle; unnegotiated carrier rejects | Default matrix passed |
| Local conversion/publication/fresh Fetch | `adopted_history_round_trips_through_hosted_publication_and_fetch`: local genesis/capture bytes and Git attribution survive hosted publication and a fresh Fetch | Targeted control: 1 passed in 7.07s; full workspace result below |

The concurrent durable-context negative and its guard-removal proof remain
pending. Part 1 owns `HostedTrust` and its production serialization guards.
Part 2 cannot replace them with a permissive adapter or edit the core crates.
Exact API requirements are in [hybrid-part2-needs.md](hybrid-part2-needs.md).

## Verify-before-install sensitivity: fail, restore, pass

Temporary mutation at checkpoint `7cbb50ab`: remove only
`self.require_locally_signed_source()?` from `StagedSource::install`. No other
behavior or test assertion changes. The original file was restored exactly.

```sh
cargo test --locked -p heddle-thread-api --lib \
  verify_before_install_rejects_without_partial_repository_mutation \
  -- --nocapture --test-threads 8
```

Guard removed; actual assertion failure:

```text
thread 'fetch::staging::tests::verify_before_install_rejects_without_partial_repository_mutation' panicked:
rejection must precede Spool mutation
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 132 filtered out; finished in 1.62s
```

Guard restored, same test and passing local-owner control:

```text
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 132 filtered out; finished in 1.78s
```

Entire Fetch staging suite after restoration:

```text
test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 118 filtered out; finished in 2.06s
```

There is no separate pre-implementation red commit. These are real temporary
production-guard removals, not a claim about TDD history. Archived local logs:
`/tmp/hybrid-part2-install-guard-{removed,restored}.log` and
`/tmp/hybrid-part2-install-control.log`.

## Real authenticated device regression

Two older assertions expected blanket protocol rejection at the daemon and
bare executor receipt admission. The client now rejects a correctly declared
HYBRID request against an old peer before Sync I/O. The receipt test verifies
its genuine original/receipt signature control, asserts `HostedTrustRequired`
without replica generation changes, and checks the real rejected stream leaves
no admitted operation. It does not enroll a transport/executor key.

```sh
cargo test --locked -p heddle-hosted-client --features client,semantic --lib \
  real_device_rpc_captures_without_weft_and_rejects_unowned_authority \
  -- --nocapture --test-threads 8
```

```text
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 455 filtered out; finished in 131.20s
```

The remaining real scenario includes genuine source publication, metadata,
capacity, ownership and device Fetch controls. The original built test failed
at its obsolete remote-protocol assertion; that failure is retained in
`/tmp/hybrid-part2-device-old-protocol-assertion.log`. This is a test migration,
not the concurrent durable-context guard-removal experiment.

## Final gates

Nightly rustfmt on touched files and `git diff --check` passed. Workspace clippy:

```sh
cargo clippy --workspace --all-targets --locked -- -D warnings
```

```text
Finished `dev` profile [unoptimized + debuginfo] target(s) in 13.86s
```

All five CI feature commands passed on the corrected test tree:

```sh
cargo clippy --workspace --all-targets --locked --features heddle-cli/telemetry -- -D warnings -D dead-code
cargo clippy --locked -p heddle-cli --no-default-features --features git-overlay,client,semantic,zstd --all-targets -- -D warnings
cargo clippy --locked -p heddle-cli --no-default-features --features native,semantic,zstd,ci --all-targets -- -D warnings
cargo check --locked -p heddle-cli --no-default-features --features git-overlay,ci
cargo check --locked -p heddle-cli --features ci
```

```text
CI FEATURE FAILURES []
```

An extra local-only CLI clippy audit (beyond CI's `cargo check` lane) found
pre-existing unused items in discuss/remote/action-line/netdaemon/resolve. It
failed and is not reported as passed. The required local-only feature check
uses CI's exact command; no warning suppression or core-file edit was made.

The remaining CI feature commands also passed:

```sh
cargo check --locked -p heddle-repo --no-default-features --features git-overlay,zstd
cargo check --locked -p heddle-repo --no-default-features --features native,zstd
cargo check --locked -p heddle-cli --no-default-features --features native,semantic,zstd
cargo clippy --locked -p heddle-cli --features ci --all-targets -- -D warnings
cargo test --locked -p heddle-cli --features ci --test ci_run_local -- --test-threads 8
```

```text
ADDITIONAL CI FEATURES PASSED
test result: ok. 31 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 10.48s
```

Both portable WASM checks passed:

```sh
cargo check --locked -p heddle-thread-api --no-default-features --target wasm32-unknown-unknown --lib
cargo check --locked -p heddle-thread-api --no-default-features --features root-attachment --target wasm32-unknown-unknown --lib
```

```text
Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.90s
Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.94s
```

The thread-api workflow's six dependency boundaries and 19 clippy/test/example
commands passed. Portable tests: 15; signing tests: 19; native tests: 129;
default library tests: 138; semantic tests: 336 with three existing ignores. No new ignore was added.
The explicitly ignored transport accounting test ran with `HEDDLE_PROFILE=1`:

```text
test one_connection_counts_all_rpc_bytes_including_failures_and_cancelled_reads ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.17s
```

The first runner invocation omitted that required environment variable and
failed its explicit prerequisite assertion. The corrected invocation passed.
The other 18 matrix commands had already passed and were not rerun needlessly.
Logs: `/tmp/hybrid-part2-feature-matrix.log`, `/tmp/hybrid-part2-matrix-15.log`,
`/tmp/hybrid-part2-ci-features.log`, `/tmp/hybrid-part2-workspace-clippy.log`,
`/tmp/hybrid-part2-wasm-{core,root}.log`.

Full workspace command, on a stable source/dependency tree with no competing
feature builds:

```sh
cargo test --workspace --locked -- --test-threads 8
```

**Passed**, including doctests, with no competing feature builds or source
changes during the run. The exact command exited zero; the runner printed:

```text
EXACT FULL WORKSPACE GATE PASSED
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

The touched hosted-client and thread-api libraries and all four hosted
publication/fresh Fetch controls passed in that same invocation:

```text
# hosted-client
test result: ok. 453 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out; finished in 213.01s
# thread-api
test result: ok. 138 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 34.83s
# CLI adoption/publication/fresh Fetch
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 173.07s
```

The preceding exact workspace run passed every CLI integration suite, then
hosted-client ended with `452 passed; 1 failed; 3 ignored`: its bearer-retention
test lacked a process-environment lock and lost its enrollment while another
test changed `HEDDLE_HOME`. The correction uses an isolated temporary home and
the existing shared environment locks. Its targeted control also passed:

```text
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 455 filtered out; finished in 0.25s
```

Archived failure: `/tmp/hybrid-part2-workspace-source-home-race.log`.
Corrected control: `/tmp/hybrid-part2-source-home-fixed.log`. Final log: `/tmp/hybrid-part2-workspace-final-tests.log`.
Earlier runs are not claimed as passed gates: one hit stale doctest dependency
artifacts after an integration merge and concurrent builds; the stable run was
stopped after a separately invoked built device test exposed an obsolete
server-side protocol rejection assertion. The latter had passed CLI 499 and
all four publication/fresh Fetch tests before being stopped. Its child Cargo
process continued after the wrapper was interrupted; its old log tail was
removed from the current run after the child exited. The current log has no
null bytes or old failure tail. That child exposed documentation corpus
freshness: `docs/llms.txt` / `docs/llms-full.txt` were regenerated from the new
source docs and the generator freshness check passed.

## Applied surfaces and remaining interfaces

- Verbs/human/agent: remote import rejects an incompatible peer before destination
  provisioning; the local conversion path and existing output modes are retained.
- Git interop: conversion continues through sley; native originals and Git
  attribution survive publication/fresh Fetch.
- Wire: existing alpha.18+ messages and protocol gates; no second view RPC.
- Reverse states: explicit job renewal and cancellation carry the original
  signed scope and CAS. Durable root replacement/high-water invalidation await
  the Part 1 receiver interface.

All observed Part 1 crypto/capability/object-model/repo interfaces are committed
on its branch but **pending in integration**. Missing atomic installer callback,
selected native branch/dependency coverage, historical transfer-prefix selection,
read-only trust snapshot and concurrent guard-removal evidence are itemized in
[hybrid-part2-needs.md](hybrid-part2-needs.md). Unresolved hosted installation and
relay remain typed closed errors. This draft is not complete HYBRID admission.
