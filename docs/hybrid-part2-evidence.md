# HYBRID Part 2 verification evidence

Branch: `task/1961-hybrid-part2-transport-client`.
PR: https://github.com/HeddleCo/heddle/pull/1963.
Production behavior checked at `c880c59dee0d791674b1210f8419c93f886da442`;
subsequent changes correct device RPC test assertions and apply nightly rustfmt.
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
Finished `dev` profile [unoptimized + debuginfo] target(s) in 30.59s
```

All five CI feature commands passed:

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

Both portable WASM checks passed:

```sh
cargo check --locked -p heddle-thread-api --no-default-features --target wasm32-unknown-unknown --lib
cargo check --locked -p heddle-thread-api --no-default-features --features root-attachment --target wasm32-unknown-unknown --lib
```

```text
Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.53s
Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.61s
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

**Pending rerun after correcting two obsolete device RPC test assertions.** Log: `/tmp/hybrid-part2-workspace-tests.log`.
Earlier runs are not claimed as passed gates: one hit stale doctest dependency
artifacts after an integration merge and concurrent builds; the stable run was
stopped after a separately invoked built device test exposed an obsolete
server-side protocol rejection assertion. The latter had passed CLI 499 and
all four publication/fresh Fetch tests before being stopped.

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
