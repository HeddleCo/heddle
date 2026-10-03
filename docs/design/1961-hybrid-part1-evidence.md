# HYBRID Part 1 verification evidence

Run in the isolated `heddle-1961` worktree on 2026-10-03, based on
`53fb89fd` with the requested alpha.18 pin. The PR targets `integration`, which
has advanced to release 0.28.7 / alpha.19 at `7f90c4ff`; these results validate
this branch, not an unrun merged feature/dependency graph. Workspace Cargo
commands unset the inherited `CARGO_TARGET_DIR` and set `TMPDIR=/home/scratch`,
so `.cargo/config.toml` selects
`/runner/heddleco-build/scratch/heddle-1961-target`. The nested CI differential
and parser probe use that same target, never a shared or replacement target.

Boundary-acceptance binding remains a typed api#318 rejection. Part 2 Fetch
routing remains separate: 38 default hosted-clone regressions (plus three
`ci,preview` cases) retain their bodies with explicit #1961 Part 2 ignores.
The new genuine HTTPS clone rejection test runs. No Part 1 negative is ignored.

## Conformance source

The three packaged fixture copies were compared byte for byte against
`git -C /home/heddleco/HeddleCo/api show v0.31.0-alpha.18:tests/fixtures/import-authority-host-witness-v1.json`.

```text
Three retained fixtures exactly match alpha.18; SHA-256
211b1a903f34635b4ae346b6ede40c2981a6cd07637265f39eb9d2020ab26577
```

Expected canonical bytes and signatures were not regenerated. Correctly signed
malformed inputs use published fixture seeds.

## Guard-removal sensitivity: fail, restore, pass

Each command ran the named existing negative and its nearby valid control.
The expected failures were test assertion failures, not compile failures.
Source and dependency files were restored exactly before final gates.

| Gate | One removed guard | Failure observed | Restored result |
| --- | --- | --- | --- |
| Role substitution | Initialize selected root/witness forbidden job keys with `Vec::new()` instead of the supplied list | `genuine device delegation cannot make witness a job signer` | 1 passed, 0 failed |
| Delegation scope | Omit `contract::verify_new_operation(operation, &current.verified, context.now)?` | `left: Ok(())`, `right: Err(Hybrid(Scope))` | 1 passed, 0 failed |
| Retirement/backdating | Omit API `verify_inclusion(&digest, proof, entry)?` | Backdated genuinely signed statement no longer returns `Contract(Proof)` | 1 passed, 0 failed |
| Revocation/retirement | Omit API `if entry.state == 3 { return Err(Reject::Revoked); }` | Identical original/proof under REVOKED no longer returns `Contract(Revoked)` | 1 passed, 0 failed |

```sh
cargo test --locked -p heddleco-capability-verifier --lib role_substitution_and_conflicting_job_associations
cargo test --locked -p heddleco-capability-verifier --lib portable_import_permission_scope_and_current_expiry
cargo test --locked -p heddle-crypto --features owner-root --lib retirement_backdating_requires_the_exact_sealed_statement
cargo test --locked -p heddle-crypto --features owner-root --lib revocation_vs_retirement_rejects_identical_original_and_cached_context
```

Every removed-guard run ended with:

```text
test result: FAILED. 0 passed; 1 failed; 0 ignored
```

Every restored-guard run ended with:

```text
test result: ok. 1 passed; 0 failed; 0 ignored
```

The two API-owned guards were mutated only in an isolated `git archive` copy of
alpha.18 under `/tmp`, temporarily selected by a local dependency patch. The
shared API checkout/cache and fixed fixture were unchanged. The workspace
manifest, lockfile and source were restored byte for byte; the final locked
build uses the original published pin. Role/scope runs were repeated against
the final expanded genuine signed negatives.

## CI features and ancillary gates

All these commands exited zero (same isolated target/environment as above):

| Command | Observed result |
| --- | --- |
| `rustfmt +nightly --edition 2024 <45 touched Rust files>`; `git diff --check` | Passed |
| `cargo check --locked -p heddle-repo --no-default-features --features git-overlay,zstd` | Passed |
| `cargo check --locked -p heddle-repo --no-default-features --features native,zstd` | Passed |
| `cargo check --locked -p heddle-cli --no-default-features --features git-overlay,client,semantic,zstd` | Passed |
| `cargo check --locked -p heddle-cli --no-default-features --features native,semantic,zstd` | Passed |
| `cargo check --locked -p heddle-cli --no-default-features --features git-overlay,ci` | Passed |
| `cargo check --locked -p heddle-cli --features ci` | Passed |
| `cargo test --locked -p heddle-cli --features ci --test ci_run_local -- --test-threads 8` | 31 passed |
| `cargo clippy --locked -p heddle-cli -p heddle-config --features heddle-cli/telemetry --lib --bins --tests --examples -- -D warnings` | Passed |
| `cargo test --locked -p heddle-cli -p heddle-config --features heddle-cli/telemetry --lib --bins -- --test-threads 1` | CLI 499, config 44, binary 5 passed |
| `cargo test --locked -p heddle-cli --features telemetry --test cli_workflow telemetry:: -- --test-threads 8` | 1 passed |
| `cargo clippy --locked -p heddle-cli --features ci --all-targets -- -D warnings` | Passed |
| `cargo test --locked -p heddleco-capability-verifier -p heddle-biscuit-verifier --all-targets -- --nocapture` | Owner verifier 72 passed, 4 existing ignores; ordinary verifier 63 unit + 1 conformance passed |
| `RUSTDOCFLAGS='-D rustdoc::broken_intra_doc_links' cargo doc --locked -p heddleco-capability-verifier -p heddle-biscuit-verifier --no-deps` | Passed; existing passkey bare-URL warning |
| `python3 .github/check_leaf_dependencies.py` | Four leaf crates passed |
| `scripts/check-publish-pipeline.sh` | Inventory, dependency order and versions passed |
| `scripts/check-hosted-leaf-consumer.sh` | Separate leaf consumer passed |
| `scripts/test-biscuit-parser-clippy.sh` | Five intended disallowed-parser diagnostics detected |
| `cargo run --locked -p heddle-devtools -- check-rust-source-reachability` | 1223 source files / 36 crates reachable; no blanket dead-code allows |
| `cargo package --locked -p heddleco-capability-verifier --list` and ordinary verifier equivalent | Both package surfaces passed |

The feature checks exposed two existing inconsistencies: the client-gated Auth
variant had an ungated catalog match arm, and `ci` depended on hosted recording
without enabling `client`. The PR fixes those feature declarations. Parallel
CLI initialization now uses the existing process-environment test lock.
The full eight-thread run also exposed the fsck renderer control racing color
toggles. Its unmodified binary passed alone; the corrected test takes the
existing `color_state` serial lock and explicitly selects uncolored output.
All 34 renderer tests then passed at eight threads. Production output is unchanged.

## WASM and published binding

```sh
cargo build --locked -p heddleco-capability-verifier --target wasm32-unknown-unknown --lib
cargo check --locked -p heddle-biscuit-verifier --target wasm32-unknown-unknown --lib
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=<wasm-bindgen-test-runner 0.2.127> cargo test --locked -p heddleco-capability-verifier --target wasm32-unknown-unknown --lib -- --nocapture
npm run build
npm run pack:binding
```

The npm steps used CI's wasm-pack 0.13.1 with writable temporary tool/npm caches.
The full WASM matrix and publish payload ended with:

```text
test result: ok. 45 passed; 0 failed; 0 ignored; 0 filtered out
npm binding version: 0.28.6
npm notice total files: 10
heddleco-capability-verifier-wasm-0.28.6.tgz
```

The exact `capability-verifier-parity.yml` seed loop, with
`OWNER_AUTH_BINDING_READY=1`, completed:

```text
OWNER_AUTH_DIFFERENTIAL=PASS seed=38322398 fuzz_cases_per_fixture=24 corpus_cases=100
OWNER_AUTH_DIFFERENTIAL=PASS seed=1138 fuzz_cases_per_fixture=24 corpus_cases=100
OWNER_AUTH_DIFFERENTIAL=PASS seed=247 fuzz_cases_per_fixture=24 corpus_cases=100
OWNER_AUTH_DIFFERENTIAL=PASS seed=836 fuzz_cases_per_fixture=24 corpus_cases=100
```

The forced negative (`OWNER_AUTH_FORCE_DIVERGENCE=1`,
`OWNER_AUTH_FUZZ_CASE_COUNT=0`) exited 1 with:

```text
OWNER_AUTH_DIFFERENTIAL_DIVERGENCE=DETECTED
```

An initial separate-workspace differential build encountered an unhashed rlib
collision between two Serde feature graphs in the same required target. After
other native builds completed, touching the verifier source forced recompilation
for the differential graph; all four seeds and the actual negative then passed.
The final workspace graph was likewise rebuilt. No target change or cargo clean.

## Targeted controls

```text
crypto import_authority: 10 passed, 0 failed
repo hosted_trust: 6 passed, 0 failed
capability-verifier import_delegation: 5 passed, 0 failed
thread-api library: 125 passed, 0 failed
real authenticated device RPC: 1 passed, 0 failed (82.54s)
authenticated HTTPS clone rejection: 1 passed, 0 failed; 38 explicitly deferred Part 2 regressions
```

The first real-device run under competing builds hit the existing capacity
mutation deadline. An isolated run reached the exact intended core rejection,
but exposed that the adapter resets its stream rather than serializing that
error. The final test checks the exact typed rejection independently, then the
real reset and no accepted operation. Its isolated run passed through capacity,
source, ownership and replication controls.

The earlier workspace run reached the 38 old hosted-clone setup failures at
`source preparation: pin hosted executor for native Thread`. The retained
Part 2 ignores and running rejection control are explicit, not a passing claim
for those clone round trips. The repo crate run also exposed an obsolete
alpha.8 requirement assertion; the alpha.18 assertion and actual public-type
verification control were corrected and passed.

The full client-feature run also reached four old native clone/pull fixtures that
expected the closed executor-pin route to install content. Their active tests
now preserve genuine framed publication and assert the exact closed-path
rejection across clone, replay, repair and hydration, with unchanged closure and
original-genesis controls. The enrollment bearer control now takes the existing
process/credential environment locks and uses an isolated device home.

Final workspace Clippy completed with exit 0 after the last receiving-test
fixes (`--workspace --all-targets --locked -- -D warnings -D dead-code`):

```text
Finished `dev` profile [unoptimized + debuginfo] target(s) in 17.96s
```

Cargo also reported the existing third-party `proc-macro-error2` future
incompatibility notice. The completed command did not suppress diagnostics.

## Final full workspace gate

After the final receiving-test corrections, the recorder created a fresh
`HEDDLE_HOME` before both commands, removed `CARGO_TARGET_DIR` from the command
environment and supplied `TMPDIR=/home/scratch`. The Cargo invocations were:

```sh
cargo clippy --workspace --all-targets --locked -- -D warnings -D dead-code
cargo test --workspace --locked -- --test-threads 8
```

Both commands completed with observed exit 0. The final workspace test command
ran for 1918.98 seconds. Summing its 176 unit/integration/doctest summaries gives
**6963 passed, 0 failed, 117 ignored**. Of the ignores, 38 are the explicitly
retained Part 2 Fetch regressions; the other 79 are existing opt-in/baseline
cases. No Part 1 negative is ignored.

The recorder completion output was:

```text
workspace-clippy-final 0
workspace-tests-final 0
```

Relevant final per-binary output tails:

```text
crypto:
test result: ok. 49 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.95s
object-model:
test result: ok. 375 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.53s
capability-verifier:
test result: ok. 72 passed; 0 failed; 4 ignored; 0 measured; 0 filtered out; finished in 3.93s
repo:
test result: ok. 986 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out; finished in 227.72s
hosted-client:
test result: ok. 446 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out; finished in 165.66s
thread-api:
test result: ok. 125 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 41.30s
renderer:
test result: ok. 34 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
hosted_clone_writes:
test result: ok. 1 passed; 0 failed; 38 ignored; 0 measured; 0 filtered out; finished in 2.54s
```

The final Cargo output tail was:

```text
all doctests ran in 0.85s; merged doctests compilation took 0.83s
   Doc-tests weft_client

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

All six new hosted-trust tests passed in that full feature graph, as did all
portable delegation, independent original/witness signature, scope, retirement,
revocation and original binding controls. The branch's clean merge-tree check
against `integration` exited 0; it is not a claim of merged-graph test execution.
