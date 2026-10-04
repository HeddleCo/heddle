# HYBRID Part 2 verification after Part 1 landed

PR: https://github.com/HeddleCo/heddle/pull/1963.
Branch: `task/1961-hybrid-part2-transport-client`.
Part 1 `f9056e6d` was plain-merged at `60eaabdd`. The client is pinned to API
alpha.21 (`05c4d08c`); its fixture is copied unchanged from that published tag.
Fixture SHA-256: `30852e349b9b17ec7ea65df18d307be760eb908f04d4721ee7dc46ca4832ded2`.

The api#318 boundary binding is verified and its transport blanket rejection
is removed. Sync's mandatory gate stays OFF at `hybrid::SYNC_MANDATORY_GATE`
until api#307. Structural staging remains distinct from admission.

## Guard-removal proofs

Both experiments use the real checks and unchanged assertions with passing
controls. They are temporary guard removals, not a claim about TDD history.

The verify-before-install test stages a genuinely signed foreign publisher and
rejects before pack, Spool, owner or replica mutation. The actual local owner
then installs successfully. Removing only
`self.require_locally_signed_source()?` from `StagedSource::install` fails:

```text
rejection must precede Spool mutation
test result: FAILED. 0 passed; 1 failed; 139 filtered out; finished in 0.83s
# same file restored exactly, same test and control
test result: ok. 1 passed; 0 failed; 139 filtered out; finished in 1.09s
```

Logs: `/tmp/hybrid-p2-v21-install-red.log` and
`/tmp/hybrid-p2-v21-install-green.log`.

The durable cached-context test uses the landed
`ThreadReplica::install_hybrid_import`. A complete unchanged published export
passes; another trust handle/thread persists N+1 witness revocation; retained N
then rejects without advancing replica generations, including after restart.
In an isolated `/tmp` source copy, bypassing only the persisted-set comparison
(`previous.as_ref()` becomes `None`) makes stale installation succeed:

```text
staged N must never authorize install after durable N+1 revocation
test result: FAILED. 0 passed; 1 failed; 139 filtered out
# same guard restored, same test
test result: ok. 1 passed; 0 failed; 139 filtered out; finished in 2.00s
# restored production HYBRID suite, including boundary and durable interleaving
test result: ok. 15 passed; 0 failed; 125 filtered out; finished in 2.05s
```

Logs: `/tmp/hybrid-p2-cached-red.log`, `/tmp/hybrid-p2-cached-green.log` and
`/tmp/hybrid-p2-hybrid-restored.log`. The original Part 1 core source was never
edited. The first original-path attempt reused the guard-removed copy's Cargo
artifact and failed; rebuilding the restored core and rerunning passed. That
first attempt is not counted as a passing gate. No cargo clean was used.

## Transport negatives and controls

| Required row | Negative and passing control |
| --- | --- |
| Completeness/protocol | `discovery_retains_peer_support_and_rejects_old_peers_before_the_gated_call`: capable peer executes; old or malformed peer never executes. Complete published bundle passes; incomplete closure rejects. |
| Verify before install | `verify_before_install_rejects_without_partial_repository_mutation`: genuine foreign publisher rejects before durable artifacts; genuine local owner installs. Guard-removal proof above. |
| Cached contexts/concurrent use | `staged_context_rechecks_concurrent_durable_revocation_before_install`: complete export installs; independently persisted N+1 invalidates retained N and remains effective after restart. Guard-removal proof above. |
| Proof-only retrieval | `retrospective_lookup_returns_only_the_exact_originals_retirement_proof`: exact public selector returns only the proof; malformed selectors never reach lookup and misses are uniform. |
| Proof substitution | `proof_substitution_cannot_authorize_a_neighboring_original`: another authentic leaf's proof rejects; exact original proof succeeds. |
| Structural staging | `structural_staging_never_authorizes_an_account_genesis` and `structural_authority_sidecars_never_authorize_native_receive`: authentic structural evidence rejects without independent trust; genuinely owned local originals are admitted. |
| Boundary binding | Published original acceptance and its exact API witness binding pass; removing the binding rejects. |
| Publication/Fetch | `adopted_history_round_trips_through_hosted_publication_and_fetch`: original genesis/capture and Git attribution survive fresh Fetch. All four controls include 140- and 1,000-state publication cases. |

The CLI clone and eight hosted exchange controls verify original local ownership,
byte-identical signed genesis/source records, and zero `hosted_executor_pins`.
Compact Fetch retains the existing object closure; replay, repair and hydration
retain local authority. Prepared-job tests pass with the published signed scope,
ordered branch limits and validity bounds: `13 passed; 0 failed`.

## Full gate

Every command uses the configured isolated target, with no competing Cargo
feature builds:

```sh
export CARGO_TARGET_DIR=/runner/heddleco-build/scratch/heddle-1961p2-target
export TMPDIR=/home/scratch
export HEDDLE_HOME=$(mktemp -d /home/scratch/hybrid-p2-home.XXXXXX)
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked -- --test-threads 8
```

Nightly rustfmt on touched files and `git diff --check` passed. Gate tails:

```text
WORKSPACE CLIPPY PASSED
EXACT FULL WORKSPACE GATE PASSED
CI FEATURES PASSED
WASM PASSED
THREAD API MATRIX PASSED
CI FEATURE FAILURES []
MATRIX FAILURES []
# workspace clippy
Finished `dev` profile [unoptimized + debuginfo] target(s) in 11.08s
# workspace hosted-client library
 test result: ok. 454 passed; 0 failed; 3 ignored; finished in 291.67s
# workspace thread-api library
 test result: ok. 140 passed; 0 failed; 0 ignored; finished in 83.15s
# workspace CLI publication / fresh Fetch
 test result: ok. 4 passed; 0 failed; 0 ignored; finished in 203.94s
# WASM core / root attachment
Finished `dev` profile [unoptimized + debuginfo] target(s) in 9.86s
Finished `dev` profile [unoptimized + debuginfo] target(s) in 16.35s
```

The exact workspace command exited zero: 176 completed suites, 7,009 passing
tests, 117 existing ignores. CI local tests: 31 passed. Telemetry CLI library:
499 passed; config: 44 passed; collector workflow: 1 passed. Matrix tests:
portable core 16, signing 20, native 131, default library 140, semantic 336,
root attachment 4, behavior analysis 2, repository replication 62, writer
leases 9 and explicit transport accounting 1. All 21 matrix commands passed.
Logs: `/tmp/hybrid-p2-v21-gates.log`, `/tmp/hybrid-p2-v21-workspace-tests.log`,
`/tmp/hybrid-p2-v21-workspace-clippy.log`, `/tmp/hybrid-p2-v21-ci-features.log`,
`/tmp/hybrid-p2-v21-extra-features.log`, `/tmp/hybrid-p2-v21-wasm-{core,root}.log`
and `/tmp/hybrid-p2-v21-matrix.log`.

The CI feature commands cover workspace telemetry, CLI Git/native/client/semantic/
zstd/CI combinations, both repo backends, focused telemetry tests and local CI
tests. WASM checks cover portable core and root attachment. The thread-api matrix
covers the workflow's six dependency boundaries and all clippy/check/test/example
commands, including explicit transport accounting with `HEDDLE_PROFILE=1`.

Two earlier resumed workspace attempts found obsolete merged expectations:
one CLI clone assertion and four hosted exchange assertions required a genuine
local-key-owned source to fail because no executor pin existed. Their corrected
controls assert original authority and zero executor enrollment. Failure logs are
`/tmp/hybrid-p2-v21-workspace-stale-clone-test.log` and
`/tmp/hybrid-p2-v21-workspace-stale-native-tests.log`; corrected targeted runs are
`/tmp/hybrid-p2-v21-local-authority.log` (`1 passed`) and
`/tmp/hybrid-p2-v21-native-exchange.log` (`8 passed`). No new ignore was added.

## Applied surfaces and outstanding work

- Verbs/human/agent: incompatible import peers reject before provisioning;
  existing text/JSON modes and local conversion remain available.
- Git interop: conversion uses sley; original signatures and Git attribution
  survive publication and fresh Fetch.
- Wire: API-owned messages, exact encodings and real boundary binding; no second
  view RPC. The Sync mandatory cutover remains OFF.
- Reverse states: explicit renewal/cancellation clients and durable trust
  invalidation. Complete selected hosted installation and relay remain pending.

Part 1's crypto, capability, object-model and repo interfaces are landed; the
exact dependencies and remaining APIs are listed in
[hybrid-part2-needs.md](hybrid-part2-needs.md). The merged installer still lacks
atomic filesystem integration and selected native closure support, and there is
no read-only trust snapshot for async refresh. Hosted installation/relay remains
typed closed errors, and 38 hosted clone/write scenarios retain their existing
ignores. Passing active tests does not complete these paths or justify removing
the PR's draft status. The owner's original file split excludes core edits; a
scope decision is pending.

Alpha.23 published during this gate at `3e66aa11`. These results belong to
alpha.21; the required repin is the next change and needs its own gate. The
dependency document records its exact client changes.
