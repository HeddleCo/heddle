# HYBRID Part 2 verification after Part 1 landed

PR: https://github.com/HeddleCo/heddle/pull/1963.
Branch: `task/1961-hybrid-part2-transport-client`.
Part 1 `f9056e6d` was plain-merged at `60eaabdd`. This first section records
the completed alpha.21 checkpoint (`e1cfb1ba`) before alpha.23 published. Its
fixture was copied unchanged from alpha.21 (`05c4d08c`). The branch is now
repinned to alpha.23; results for that repin are recorded separately below.
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
alpha.21; the following section records the completed repin and its separate
gate. The dependency document records the exact client changes.

## Alpha.23 repin and current verification

API alpha.23 published during the resumed gate. The required repin selects
`3e66aa11e93a47692004fa1d8c2fdb11991a3f32` in all API manifests and the lockfile,
with one Git API dependency and no unrelated lockfile changes. The complete
unmodified transport/client fixture is now `hybrid-alpha23.json`, SHA-256
`ee4c25afb126aff7a2d5d9abd340b04f5b8f37c4cfa8eea13ed61db5651bf53d`.

The client lifecycle uses authenticated configuration discovery, exact caller
scope/token issuance and typed refusal, Commit-only source/proof submission,
lineage-bound receipt validation, the active host-issued cancellation ID,
non-executable predecessor recovery and advertised browser skew without expiry
grace. Sley discovery retains known OIDs; private-provider callers can supply
independent target knowledge, or explicit unavailable knowledge requiring signed
observe disclosure. No generic capture credential becomes import permission.

The alpha.22 wire additions also require metadata settings masks and effective
ResolveResources budget echoes. Metadata revisions leave settings untouched;
local settings patches apply only selected top-level fields under the existing
catalog transaction. The integration test covers unselected values, selected
values, malformed masks and rejection without advancing CAS.

Actual alpha.23 gate tails:

```text
cargo clippy --workspace --all-targets --locked -- -D warnings
Finished `dev` profile [unoptimized + debuginfo] target(s) in 1m 25s
CI FEATURE FAILURES []
ADDITIONAL CI FEATURES PASSED
WASM CORE EXIT 0
WASM ROOT EXIT 0
# thread-api default tests
140 passed; 0 failed; 0 ignored
# thread-api native feature tests
131 passed; 0 failed; 0 ignored
# complete feature matrix: 20/21 commands pass
MATRIX FAILURES [16]
# matrix command 16: untouched Part 1 repository fixture tests
56 passed; 6 failed; 0 ignored
# exact workspace command stops in untouched Part 1 crypto fixtures
53 passed; 12 failed; 0 ignored
# targeted capability import-delegation fixtures
0 passed; 8 failed; 0 ignored
```

The workspace run completed 74 suites with 2,551 passing tests and 79 existing
ignores before stopping in crypto. The CLI hosted publication/fresh Fetch suite
passed all four tests, including 140 and 1,000 States. Git discovery and source
advertisement-budget tests pass. All five CI feature commands, additional CI
checks/tests, telemetry and both WASM builds pass. Every thread-api matrix command
passes except the repository command that consumes the old core fixture.

The core failures are visible and **not waived**. Crypto/capability tests report
`Hybrid(Signature)` on their old signed scopes; repository controls report
`Hybrid(ImportPermission)` or mismatched old expectations. Those three fixtures
remain alpha.21 under the original Part 1 file split. They need the coordinated
alpha.23 refresh and any amended test expectations from the Part 1 owner.

The first hosted-client run found a stale cleanup CAS in the newly expanded
settings-mask test; that test was corrected. The corrected full run passes that
control and 462 tests, with three existing ignores, but one existing device RPC
capacity test hits its mutation deadline with forty live views. Its isolated
rerun also fails the unchanged five-second mutation deadline:

```text
real_device_rpc_captures_without_weft_and_rejects_unowned_authority
FAILED. 0 passed; 1 failed; 465 filtered out; finished in 150.11s
```

Neither timeout nor core failure is hidden. The active request/retained-view
permit assertions pass before the timeout; this does not establish its cause.
The alpha.21 workspace gate passed this same capacity test. No deadline has been
increased and no performance failure is waived.

Logs: `/tmp/hybrid-p2-v23-workspace-tests.log`,
`/tmp/hybrid-p2-v23-final-clippy.log`, `/tmp/hybrid-p2-v23-ci-features.log`,
`/tmp/hybrid-p2-v23-extra-features.log`, `/tmp/hybrid-p2-v23-matrix.log`,
`/tmp/hybrid-p2-v23-hosted-final-tests.log`,
`/tmp/hybrid-p2-v23-capability-tests.log`,
`/tmp/hybrid-p2-v23-device-rerun.log`.

## Alpha.23 guard-removal reruns

Both tests and passing controls now consume the actual published alpha.23
fixture. The durable experiment edits only the isolated `/tmp` source copy;
production core files remain unchanged. The verify-before-install experiment
restores the owned source file byte-for-byte in `finally` before its green run.

```text
# durable cached-context guard removed
staged N must never authorize install after durable N+1 revocation
FAILED. 0 passed; 1 failed; 139 filtered out; finished in 5.03s
# exact guard restored
ok. 1 passed; 0 failed; 139 filtered out; finished in 2.62s
# original restored HYBRID suite
ok. 19 passed; 0 failed; 121 filtered out; finished in 3.00s

# verify-before-install guard removed
rejection must precede Spool mutation
FAILED. 0 passed; 1 failed; 139 filtered out; finished in 0.73s
# exact guard restored
ok. 1 passed; 0 failed; 139 filtered out; finished in 1.33s
```

Logs: `/tmp/hybrid-p2-v23-cached-{red,green}.log`,
`/tmp/hybrid-p2-v23-hybrid-restored.log`,
`/tmp/hybrid-p2-v23-install-{red,green}.log`.
Restored core SHA-256 (original and copy):
`122f763fb3e2e450ce1e540e9edc51762fd7114f78a660b2202a3ab69d1017b8`.

PR #1963 remains draft: the exact full alpha.23 gate is not green, and the
selected hosted installer, atomic staged filesystem installation and read-only
trust snapshot interfaces in `hybrid-part2-needs.md` remain pending.

The new `task/1961-hybrid-part1b-core-seams` worktree contains uncommitted drafts
for those interfaces. Its branch still points at integration `f9056e6d`; Part 2
will consume committed public APIs and plain-merge when they land. Draft source
is not reported as a landed dependency or passing production installation.
