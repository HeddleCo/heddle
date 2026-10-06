# HYBRID Part 10: Fetch foreign cuts and receiver clock sampling

Fetch recognizes an exact foreign original before loading its causal parents.
It records that original as a structural endpoint and stops the traversal there,
including during `StagedSource::select_prefix` and claim-cutoff bookkeeping.
Native originals retain their ordinary exact ancestry validation. Staging confers no authority: hosted install
still resolves every foreign dependency through the exact retained prefix under
current receiver trust, with the existing `NativeClosure` verifier.

The host Fetch contract is the selected native ancestry (including integration
source edges and required native claim cutoffs), dependency Genesis records,
and each exact `ForeignDependencyV1` endpoint original. Stop causal traversal
at those exact endpoints. Do not include ancestry behind them merely to satisfy
native staging; unused originals are refused. The host still supplies the
selected State's actual content/reference closure in its pack and index.
A fresh receiver fetches and installs each foreign origin's exact admission
prefix separately before admitting the dependent native closure. That separate
prefix carries the original ancestry and import certificates needed by its own
origin verifier; it does not turn that ancestry into native causal history.
An unmatched signed digest or Thread identity receives no endpoint exception.

Each wall reading is now bracketed by monotonic readings. Rollback compares the
wall difference with the minimum elapsed time outside both brackets. Scheduling
inside either bracket has no fixed bound and cannot masquerade as rollback.
The existing one-millisecond truncation bound remains; signed expiry is never
extended. Monotonic regression inside a bracket or between brackets refuses,
as does an observed wall rollback or time below the durable SQLite floor.
Snapshot, mutation, installation, commit and final access sampling all use the
same internal sampler. There are no retries, sleeps or wider clock tolerance.

## Consumer API

The `Clock` trait and all staging/installation signatures are unchanged.
The sampler reads `elapsed_millis` twice around each `now_millis` call; injectable
clocks must preserve their shared monotonic epoch across both readings:

```rust
pub trait Clock: Send + Sync {
    fn now_millis(&self) -> repo::thread_replication::Result<i64>;
    fn elapsed_millis(&self) -> repo::thread_replication::Result<u64>;
}
```

`thread_api::fetch::Error::Repository(Box<repo::thread_replication::Error>)`
is removed. With the `native` feature, Fetch carries the small typed projection
required at its protocol boundary instead:

```rust
thread_api::fetch::Error::ForeignPrefixLimitExceeded {
    limit_name: &'static str,
    limit: usize,
}
impl From<repo::thread_replication::Error> for thread_api::fetch::Error
```

That conversion preserves repository prefix-limit fields; other repository
failures become `Preparation(String)`. Weft callers matching the former boxed
variant must match this variant directly. The hosted-client conversion preserves
`ProtocolError::ForeignPrefixLimitExceeded { limit_name, limit }` without a heap
box. Wire types, crypto and capability verifier code are unchanged.

## Regression evidence

The original Fetch path failed the actual Ready/operations/pack/index/Complete
receiver test with `Invalid("incomplete source ancestry")` while loading omitted
foreign parents. Its controls cover native fast-forward and merge landing;
the bidirectional continuation case exercises a foreign original with a causal
Git ancestor and also selects its exact native prefix before hosted install.
The same test passes after the endpoint cut, including fresh receiver prefix
installation and the native hosted installation.

The original clock path failed
`receiver_clock_sampling_delay_does_not_report_rollback` at
`sampling delay must not masquerade as rollback: HostedClock` (exit 101).
The bracketed sampler passes delayed wall and monotonic sampling, including an
arbitrary 4,000,000,000 ms scheduling gap during snapshot history reads.
Real wall and monotonic rollback remain typed `HostedClock` refusals.

The isolated guard runner verified **12 fail-then-pass pairs**. Every red run
exited 101 at its named runtime assertion. Every restored run exited 0 with one
passing test; compiler failures and empty selections are rejected by the runner.
The first eleven pairs used committed source
`0884335dcd84ee6adca5089b4e2f5323a455e6dd`; the claim-cutoff pair used
`95155c3bc02e8f6d66eaae71ad71d9e89ecbe31d`. Source files were restored byte-for-byte.
Commit `d03e1e84727e7727beac329b6e86d7b64b5de2aa` subsequently strengthens
two test-only controls. Observed wall rollback happens above the durable floor,
so removing the progress guard
actually admits it rather than merely changing its error type. The forged
endpoint case also omits its Git ancestor, so weakening exact digest matching
actually stages that graph rather than refusing unused ancestry. The supplied
ancestor case still requires the exact native ancestry refusal. The refined
pairs also pass against that commit: both mutated paths wrongly accept
(`None` for staged Fetch error and `Ok(())` for wall rollback), then restoration
returns the intended refusal. This makes **14 verified pair executions across
12 unique guards**. Affected tests, affected/workspace/CI-feature clippy,
workspace tests and formatting were rerun afterward; all passed. A redundant Vec
conversion was also removed for clippy.

| Guard | Runtime test | Red → restored |
| --- | --- | --- |
| `foreign-fetch-before-parents` | `fresh_fetch_native_main_with_landed_import_stops_at_exact_foreign_endpoint` | 101 → 0, 1 passed |
| `foreign-fetch-past-endpoint` | `fresh_fetch_native_main_with_landed_import_stops_at_exact_foreign_endpoint` | 101 → 0, 1 passed |
| `foreign-prefix-endpoint-cut` | `fresh_fetch_native_main_with_landed_import_stops_at_exact_foreign_endpoint` | 101 → 0, 1 passed |
| `foreign-fetch-exact-digest` | `forged_foreign_endpoint_is_traversed_and_rejected` | 101 → 0, 1 passed |
| `foreign-fetch-native-ancestry` | `native_fetch_mismatched_ancestry_is_rejected` | 101 → 0, 1 passed |
| `receiver-clock-sampling-interval` | `receiver_clock_sampling_delay_does_not_report_rollback` | 101 → 0, 1 passed |
| `receiver-clock-wall-rollback` | `receiver_clock_wall_rollback_is_refused` | 101 → 0, 1 passed |
| `receiver-clock-monotonic-regression` | `receiver_clock_monotonic_regression_is_refused` | 101 → 0, 1 passed |
| `receiver-clock-intrasample-regression` | `receiver_clock_monotonic_regression_within_sample_is_refused` | 101 → 0, 1 passed |
| `foreign-replay-error-type` | `foreign_prefix_replay_depth_33_refuses_with_typed_limit` | 101 → 0, 1 passed |
| `hosted-replay-error-type` | `hosted_replay_preserves_foreign_prefix_limit_type` | 101 → 0, 1 passed |
| `foreign-fetch-claim-cutoff` | `source_staging_retains_signed_claim_cutoff_beyond_selected_revision` | 101 → 0, 1 passed |

Reproduce all twelve pairs from a committed tree:

```sh
export TMPDIR=/home/scratch
export CARGO_TARGET_DIR=/runner/heddleco-build/scratch/heddle-part10-target
export HEDDLE_HOME=$(mktemp -d)
python3 scripts/prove-native-witness-guards.py --output "$(mktemp -d)" \
  foreign-fetch-before-parents foreign-fetch-past-endpoint \
  foreign-prefix-endpoint-cut foreign-fetch-exact-digest \
  foreign-fetch-native-ancestry foreign-fetch-claim-cutoff \
  receiver-clock-sampling-interval receiver-clock-wall-rollback \
  receiver-clock-monotonic-regression receiver-clock-intrasample-regression \
  foreign-replay-error-type hosted-replay-error-type
```

The claim-cutoff staging control verifies both native cutoff retention/refusal
and foreign endpoint deferral: native history still requires the signed cutoff
original; an exact foreign endpoint's cutoff remains the other origin's problem.
The retained receiver verifier still authenticates that prefix at install.

Selected actual regression output:

```text
Fetch must stop before loading foreign parents: Invalid("incomplete source ancestry")
test hybrid::foreign_tests::fresh_fetch_native_main_with_landed_import_stops_at_exact_foreign_endpoint ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 212 filtered out

sampling delay must not masquerade as rollback: HostedClock
test thread_replication::hosted_trust_tests::receiver_clock_sampling_delay_does_not_report_rollback ... FAILED

test hybrid::foreign_tests::fresh_fetch_native_main_with_landed_import_stops_at_exact_foreign_endpoint ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 212 filtered out

test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 970 filtered out
```

## Final verification

All **41/41 Part 9 final gates pass**, using the same commands and feature
configurations. The final receipts contain **10,232 passing Cargo/nextest test
executions**, **87 existing ignores/skips**, and **zero failures**. Counts span
overlapping feature configurations rather than unique tests. The 73 WASM tests
are included; bigint/differential checks are counted separately.

Every gate used a fresh `HEDDLE_HOME=$(mktemp -d)`,
`TMPDIR=/home/scratch`, and
`CARGO_TARGET_DIR=/runner/heddleco-build/scratch/heddle-part10-target`.
Cargo commands used `--offline --locked`. Both WASM tools used
`wasm-bindgen 0.2.127`. Gates ran serially, without retries or timeout changes.
The CLI 1,000-State publication/later-capture workload passed in **334.041
seconds**, within its unchanged **360-second** timeout.

All four seeds (`38322398`, `1138`, `247`, `836`) matched **277 Rust/WASM
cases each**, **1,108 differential cases total**. Each seed passed **374 bigint
ABI checks**, **1,496 across four runs**. Forced divergence exited 1 with
`OWNER_AUTH_DIFFERENTIAL_DIVERGENCE=DETECTED`; that deliberate refusal is the
required passing control, not a failed final gate.

| Runtime gate | Passed | Ignored/skipped |
| --- | ---: | ---: |
| `affected-tests` | 1,367 | 11 |
| `workspace-tests` | 4,622 | 25 |
| `cli-serialized-units` | 505 | 0 |
| `cli-ci-integration` | 31 | 0 |
| `cli-ci-suite` | 2,095 | 41 |
| `hosted-client` | 491 | 3 |
| `thread-default` | 252 | 1 |
| `thread-signing` | 20 | 0 |
| `thread-root-attachment` | 4 | 0 |
| `thread-core` | 16 | 0 |
| `thread-native` | 204 | 0 |
| `thread-iroh-replication` | 107 | 1 |
| `semantic` | 336 | 3 |
| `thread-behavior` | 2 | 0 |
| `thread-transport-observation` | 1 | 0 |
| `repo-thread-matrix` | 97 | 2 |
| `objects-writer-lease` | 9 | 0 |
| `capability-wasm-tests` | 73 | 0 |

Selected actual final terminal output:

```text
test result: ok. 74 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 18.73s
test result: ok. 969 passed; 0 failed; 5 ignored; 0 measured; 0 filtered out; finished in 70.25s
test result: ok. 213 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 240.99s
test result: ok. 111 passed; 0 failed; 6 ignored; 0 measured; 0 filtered out; finished in 8.83s
     Summary [1326.926s] 2095 tests run: 2095 passed (3 slow), 41 skipped
     Summary [  75.391s] 491 tests run: 491 passed (2 slow), 3 skipped
test result: ok. 73 passed; 0 failed; 0 ignored; 0 filtered out; finished in 7.87s
ℹ tests 374
ℹ pass 374
ℹ fail 0
OWNER_AUTH_DIFFERENTIAL=PASS seed=38322398 fuzz_cases_per_fixture=24 corpus_cases=277
ℹ tests 374
ℹ pass 374
ℹ fail 0
OWNER_AUTH_DIFFERENTIAL=PASS seed=1138 fuzz_cases_per_fixture=24 corpus_cases=277
ℹ tests 374
ℹ pass 374
ℹ fail 0
OWNER_AUTH_DIFFERENTIAL=PASS seed=247 fuzz_cases_per_fixture=24 corpus_cases=277
ℹ tests 374
ℹ pass 374
ℹ fail 0
OWNER_AUTH_DIFFERENTIAL=PASS seed=836 fuzz_cases_per_fixture=24 corpus_cases=277
ℹ tests 374
ℹ pass 374
ℹ fail 0
OWNER_AUTH_DIFFERENTIAL_DIVERGENCE=DETECTED seed=38322398 count=1
```

The two refined guard failures are genuine admission mistakes when guards are
removed, followed by successful restored refusals:

```text
unverified endpoint must traverse and retain native ancestry checks: None
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 212 filtered out; finished in 0.19s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 212 filtered out; finished in 0.41s
real wall rollback must refuse: Ok(())
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 973 filtered out; finished in 0.47s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 973 filtered out; finished in 0.49s
```

Receipts and named logs:

- `/tmp/heddle-part10-evidence/final/results.json`: final 41 command/count receipts.
- `/tmp/heddle-part10-evidence/guards/results.json`: initial eleven pairs.
- `/tmp/heddle-part10-evidence/claims-guard/results.json`: claim-cutoff pair.
- `/tmp/heddle-part10-evidence/refined-guards/results.json`: two strengthened pairs.
- `/tmp/heddle-part10-evidence/before-test-refinement/`: first six original gate
  logs and receipts, retained before their final rerun.

Complete final command inventory (all PASS):

| # | Gate | Exact command |
| ---: | --- | --- |
| 1 | `fmt` | `python3 /tmp/heddle-part10-evidence/fmt-touched.py` |
| 2 | `affected-tests` | `cargo test --offline --locked -p heddle-crypto -p heddle-repo -p heddle-thread-api -p heddleco-capability-verifier --lib` |
| 3 | `affected-clippy` | `cargo clippy --offline --locked -p heddle-crypto -p heddle-repo -p heddle-thread-api -p heddle-hosted-client -p heddleco-capability-verifier --all-targets --features heddle-hosted-client/client -- -D warnings -D dead-code` |
| 4 | `workspace-clippy` | `cargo clippy --offline --locked --workspace --all-targets -- -D warnings -D dead-code` |
| 5 | `ci-feature-clippy` | `cargo clippy --offline --locked -p heddle-cli --features ci --all-targets -- -D warnings -D dead-code` |
| 6 | `workspace-tests` | `cargo test --offline --locked --workspace --exclude heddle-cli` |
| 7 | `cli-serialized-units` | `cargo test --offline --locked -p heddle-cli --lib --bins -- --test-threads=1` |
| 8 | `cli-ci-integration` | `cargo test --offline --locked -p heddle-cli --features ci --test ci_run_local` |
| 9 | `cli-ci-suite` | `cargo nextest run --offline --locked -p heddle-cli --features client --test-threads 4` |
| 10 | `hosted-client` | `cargo nextest run --offline --locked -p heddle-hosted-client --features client` |
| 11 | `thread-wasm-core` | `cargo check --offline --locked -p heddle-thread-api --no-default-features --target wasm32-unknown-unknown --lib` |
| 12 | `thread-wasm-attachment` | `cargo check --offline --locked -p heddle-thread-api --no-default-features --features root-attachment --target wasm32-unknown-unknown --lib` |
| 13 | `thread-dependency-boundaries` | `python3 /tmp/heddle-part10-evidence/thread-boundaries.py` |
| 14 | `thread-default` | `cargo test --offline --locked -p heddle-thread-api` |
| 15 | `thread-signing` | `cargo test --offline --locked -p heddle-thread-api --no-default-features --features signing --lib` |
| 16 | `thread-root-attachment` | `cargo test --offline --locked -p heddle-thread-api --no-default-features --features root-attachment --test root_attachment` |
| 17 | `thread-root-clippy` | `cargo clippy --offline --locked -p heddle-thread-api --no-default-features --features root-attachment --all-targets -- -D warnings` |
| 18 | `thread-replication-check` | `cargo check --offline --locked -p heddle-thread-api --no-default-features --features replication --lib` |
| 19 | `thread-core` | `cargo test --offline --locked -p heddle-thread-api --no-default-features --lib` |
| 20 | `thread-native` | `cargo test --offline --locked -p heddle-thread-api --no-default-features --features native --lib` |
| 21 | `thread-iroh-replication` | `cargo test --offline --locked -p heddle-thread-api --no-default-features --features iroh,replication` |
| 22 | `semantic` | `cargo test --offline --locked -p heddle-semantic --lib` |
| 23 | `thread-behavior` | `cargo test --offline --locked -p heddle-thread-api --no-default-features --features semantic-analysis --test behavior_analysis` |
| 24 | `thread-behavior-clippy` | `cargo clippy --offline --locked -p heddle-thread-api --no-default-features --features semantic-analysis --all-targets -- -D warnings` |
| 25 | `thread-transport-observation` | `cargo test --offline --locked -p heddle-thread-api --no-default-features --features iroh,replication --test transport_observation -- --ignored --nocapture` |
| 26 | `repo-thread-matrix` | `cargo test --offline --locked -p heddle-repo --no-default-features --features native,git-overlay thread_replication -- --nocapture` |
| 27 | `objects-writer-lease` | `cargo test --offline --locked -p heddle-objects writer_lease -- --nocapture` |
| 28 | `thread-clippy` | `cargo clippy --offline --locked -p heddle-thread-api --all-targets -- -D warnings` |
| 29 | `thread-core-clippy` | `cargo clippy --offline --locked -p heddle-thread-api --no-default-features --all-targets -- -D warnings` |
| 30 | `thread-native-clippy` | `cargo clippy --offline --locked -p heddle-thread-api --no-default-features --features native --all-targets -- -D warnings` |
| 31 | `thread-signing-clippy` | `cargo clippy --offline --locked -p heddle-thread-api --no-default-features --features signing --lib -- -D warnings` |
| 32 | `thread-replication-clippy` | `cargo clippy --offline --locked -p heddle-thread-api --no-default-features --features replication --lib -- -D warnings` |
| 33 | `thread-iroh-clippy` | `cargo clippy --offline --locked -p heddle-thread-api --no-default-features --features iroh,replication --all-targets -- -D warnings` |
| 34 | `thread-example` | `cargo run --offline --locked -p heddle-thread-api --example thread` |
| 35 | `capability-wasm-build` | `cargo build --offline --locked -p heddleco-capability-verifier --target wasm32-unknown-unknown --lib` |
| 36 | `biscuit-wasm-check` | `cargo check --offline --locked -p heddle-biscuit-verifier --target wasm32-unknown-unknown --lib` |
| 37 | `capability-wasm-tests` | `cargo test --offline --locked -p heddleco-capability-verifier --target wasm32-unknown-unknown --lib -- --nocapture` |
| 38 | `npm-binding` | `npm run build --prefix crates/capability-verifier` |
| 39 | `npm-pack` | `npm run pack:binding --prefix crates/capability-verifier` |
| 40 | `bigint-differential` | `python3 /tmp/heddle-part10-evidence/differential-seeds.py` |
| 41 | `forced-differential` | `bash crates/capability-verifier/owner-authorization-conformance/run.sh` |

The three small command wrappers used above are reproduced here. Formatting
checks the eight changed Rust files. Dependency boundaries assert the same
Part 9 normal-dependency exclusions. The differential wrapper iterates the
committed seed list.

`fmt-touched.py`:

```python
import subprocess
paths=subprocess.check_output(['git','diff','--name-only','origin/integration','--','*.rs'],text=True).splitlines()
rs=sorted(set(paths))
assert rs, 'No touched Rust files selected'
subprocess.run(['rustfmt','+nightly','--edition','2024','--config','skip_children=true','--check',*rs],check=True)
print(f'{len(rs)} touched Rust files pass nightly rustfmt')
```

`thread-boundaries.py`:

```python
import subprocess
cases = [
    ("", {"heddle-repo", "heddle-objects", "heddleco-iroh", "biscuit-auth", "tokio"}),
    ("native", {"heddleco-iroh"}),
    ("root-attachment", {"heddle-repo", "heddle-objects", "heddleco-iroh", "tokio"}),
    ("signing", {"heddle-repo", "heddle-objects", "heddleco-iroh", "biscuit-auth", "tokio"}),
    ("replication", {"heddle-repo", "heddle-objects", "heddleco-iroh", "biscuit-auth"}),
    ("iroh", {"heddle-repo", "heddle-objects", "biscuit-auth"}),
]
for features, forbidden in cases:
    if features != "native": forbidden |= {"heddle-semantic", "tree-sitter"}
    command = ["cargo", "tree", "--offline", "--locked", "-p", "heddle-thread-api", "--no-default-features", "--edges", "normal", "--prefix", "none", "--format", "{p}"]
    if features: command += ["--features", features]
    names = {line.split()[0] for line in subprocess.check_output(command, text=True).splitlines()}
    assert {"heddle-thread-api", "heddle-api", "prost"} <= names
    assert not names & forbidden, (features, names & forbidden)
    print(f"{features or 'core'}: dependency boundary holds")
```

`differential-seeds.py`:

```python
import os, subprocess
from pathlib import Path
for seed in Path('crates/capability-verifier/owner-authorization-conformance/seeds.txt').read_text().split():
 env=os.environ.copy();env['OWNER_AUTH_CASE_SEED']=seed
 subprocess.run(['bash','crates/capability-verifier/owner-authorization-conformance/run.sh'],env=env,check=True)
```

Browser/conformance commands additionally set:

```sh
export PATH=/home/scratch/wasm-bindgen-127/bin:$PATH
export CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner
export OWNER_AUTH_BINDING_READY=1
export OWNER_AUTH_TARGET=/runner/heddleco-build/scratch/heddle-part10-target/owner-auth
export OWNER_AUTH_SCRATCH=/tmp/heddle-part10-evidence/final/corpus
```

The transport-observation gate sets `HEDDLE_PROFILE=1`. The forced-divergence
gate additionally sets `OWNER_AUTH_FORCE_DIVERGENCE=1` and
`OWNER_AUTH_FUZZ_CASE_COUNT=0`. Npm uses
`NPM_CONFIG_CACHE=/home/scratch/heddle-part9-evidence/npm-cache`.

## CI scheduling

The first GitHub Thread API job was canceled at its existing 25-minute
job budget, during compilation of the iroh replication configuration. Its
default suite passed all 252 tests (one existing ignore), and its native-only
suite passed all 204 tests. The two library runs took 511.54 and 507.00 seconds,
respectively; compilation and the remaining serial feature checks exhausted
the job's budget. No runtime assertion failed.

The workflow now runs those two exact test commands in separate default/native
matrix jobs. The original `thread-client` check retains the portable,
replication, semantic, repository and feature-clippy commands. All 23 original
run steps are preserved once per intended configuration, the existing
25-minute budget is unchanged for every job, and neither test selection nor
production behavior changes. YAML parsing and command-multiset comparison
verified the partition before push. Matrix fail-fast is disabled so both
configurations finish independently.

First-run logs are retained in
`/tmp/heddle-part10-evidence/thread-ci-first.log`, with GitHub's annotation
`The job has exceeded the maximum execution time of 25m0s`.

```text
test result: ok. 213 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 511.54s
test result: ok. 204 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 507.00s
```

## Surfaces

Verb/help/clap and human/agent output contracts are unchanged. This is a native
receiver validation change; Git import/export/projection continues through sley.
No wire fields or new RPCs are added. Forged-reference, native ancestry, wall
rollback, monotonic rollback and retained-prefix refusal controls cover the
reverse states. No production unwrap/expect, fallback, compatibility shim or
trait-object dispatch is introduced.
