# HYBRID Part 1b: core seams for #1963

Part of #1961; seams for #1963. Core changes are confined to `crates/repo`.
The API remains integration's alpha.21 pin, commit
`05c4d08c4e7120c2532a2b2a6c608b12e58acc4b`; Part 2 owns its repin.
The three published core fixture copies remain unchanged.

## Public signatures

```rust
ThreadReplica::install_hybrid_import(
    directory: &Path,
    trust: &HostedTrust<impl Clock>,
    bundle_bytes: &[u8],
    native_records: &[wire::SignedRecord],
    authority: &impl AcceptedAuthority,
    store: &impl ObjectStore,
    before_commit: impl FnOnce(&mut InstallArtifacts) -> Result<()>,
) -> Result<Vec<ThreadReplica>>;

// repo::thread_replication::install_artifacts
impl InstallArtifacts {
    pub fn install_file(&mut self, staged: &Path, destination: &Path) -> Result<()>;
    pub fn write_file(&mut self, destination: &Path, bytes: &[u8]) -> Result<()>;
}

// repo::thread_replication::hosted_trust
pub struct TrustSnapshot {
    pub root: RootSelection,
    pub root_epoch: u64,
    pub previous: Option<api::witness_trust::VerifiedWitnessSet>,
    pub clock_floor_millis: i64,
    pub known_job_associations: Vec<(Vec<u8>, Vec<u8>)>,
}
impl<C: Clock> HostedTrust<C> {
    pub fn snapshot(&self) -> Result<TrustSnapshot>;
}
```

The callback is the permitted equivalent seam: it receives the filesystem
transaction that owns its pack, pin and metadata writes. An unrestricted
zero-argument callback cannot tell core which arbitrary filesystem writes to
undo. Callers must use the journal for actual artifact installation and supply
an isolated staging object store for receives. Destination parents already
exist; SQLite remains owned by the trust transaction. Existing files retain
rollback copies without copying large packs into memory. Repeated writes
restore the first previous version. New files are removed on rejection.

Complete public delegation, publication, renewal, owner and slot histories are
verified even when their converted counterparts are absent. `native_records`
selects actual originals; the installer follows their signed causal dependencies
and genesis, requires exact per-original admission coverage, and receives in
dependency order. It never creates an unrelated carried branch. Original-author
witnesses, landing evidence and individually authenticated boundary receipts can
cover native dependencies; an unproved dependency still rejects. Retained public
bundles are unchanged.

All verification and selected receives precede the callback. Receiver time and
current disclosure are checked immediately before it. The same trust mutex and
SQL IMMEDIATE transaction remain held. Callback errors, final receiver-clock or
set-expiry failures, final disclosure rejection and SQL commit errors restore
journaled artifacts. Authority rejection restores files before dropping the SQL
transaction and shared trust lock.

`snapshot` uses a read-only SQLite transaction. It restores the original signed
set under the retained history root, returns the current selected root/epoch
separately, and leaves durable clock/set/job state unchanged. Expired history is
only a floor; `mutate` still reloads and re-verifies fresh trust at actual use.
Durable wall floors and shared process monotonic anchors reject rollback.

## Executed guard-removal proofs

An independent `/tmp/1961p1b-guard-removal` source copy was used; the working tree,
shared dependency sources and Part 2 worktree were not mutated. Logs are in
`/tmp/1961p1b-evidence`. Each removed guard exited 101 with **0 passed, 1 failed**;
each restored control exited 0 with **1 passed, 0 failed**.

| Regression | Isolated mutation | Observed failure |
| --- | --- | --- |
| `hybrid_late_rejection_restores_pack_pins_metadata_and_sql` | Remove filesystem rollback from rejection paths | Installed pack survives callback rejection. Restored run also executes expiry, clock rollback and final disclosure rejection, then successful installs. |
| `hybrid_selected_genesis_does_not_install_unrelated_carried_branch` | Replace explicit native selection with all carried originals | Two branches installed instead of one. |
| `hybrid_uncovered_original_rejects_before_callback` | Remove exact per-original admission coverage gate | A genuinely signed, uncovered native control installs. |
| `hybrid_snapshot_does_not_mutate_floor_and_restores_retained_root_history` | Breach the read-only seam by routing stored set through `mutate` before reading | Reading advances durable floor from 1350000 to 1350100. |
| `hybrid_snapshot_rejects_clock_below_durable_floor` | Remove snapshot's durable receiver-clock floor check | A restarted receiver accepts time below its persisted floor. |

Additional executed controls cover selected capture/genesis dependencies,
unselected public-history tampering, independent native control admission,
boundary receipt dependencies, original genesis sidecar retention, expired
history after explicit root replacement, job associations, frozen clocks across
handles, and an independent SQL writer blocked inside the callback.

Final targeted command:

```sh
CARGO_TARGET_DIR=/runner/heddleco-build/scratch/heddle-1961p1b-target \
  cargo test --locked -p heddle-repo --lib hosted_trust_tests -- --test-threads 8
```

Executed output:

```text
running 19 tests
test result: ok. 19 passed; 0 failed; 0 ignored; 0 measured; 926 filtered out; finished in 31.42s
```

For each of the five named guard-removal runs above, the same test command in
the isolated source copy produced these outputs before and after restoration:

```text
removed:  test result: FAILED. 0 passed; 1 failed; 0 ignored
restored: test result: ok. 1 passed; 0 failed; 0 ignored
```

## Gates

All commands exited 0. Workspace coverage used a fresh `HEDDLE_HOME` and CI's
partitioned CLI execution: CLI unit tests serialize process-global environment
changes, while the other suites use eight test threads.

```sh
export HEDDLE_HOME=$(mktemp -d)
cargo test --workspace --exclude heddle-cli --locked -- --test-threads 8
cargo test --locked -p heddle-cli --lib --bins -- --test-threads 1
cargo test --locked -p heddle-cli --features ci --test '*' -- --test-threads 8
cargo test --locked -p heddle-cli --doc

cargo clippy --workspace --all-targets --locked -- -D warnings -D dead-code
cargo clippy --workspace --all-targets --locked --features heddle-cli/ci -- -D warnings -D dead-code
```

| Gate | Executed result |
| --- | --- |
| `rustfmt +nightly --edition 2024` on all eight touched Rust files | Passed |
| Non-CLI workspace tests, including doc tests | 4,458 passed; 0 failed; 21 ignored |
| CLI unit/bin tests, serialized as in CI | 504 passed; 0 failed |
| CLI integration tests with required `ci` feature | 1,578 passed; 0 failed; 79 ignored |
| CLI doc tests | 0 tests; passed |
| Final targeted hosted-trust/installer tests | 19 passed; 0 failed |
| Workspace Clippy, all targets | Passed |
| Workspace Clippy with `heddle-cli/ci`, all targets | Passed |
| `git diff --check` and staged whitespace check | Passed |

The workspace's complete repo library run produced:

```text
test result: ok. 999 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out; finished in 169.86s
```

The installer frontier/dependency indexing optimization followed that broad run;
all 19 targeted tests, all five isolated guard-removal pairs and both Clippy
variants were then rerun against the final Rust source. CLI integrations also
ran against that source. Logs are retained in `/tmp/1961p1b-evidence`.

An initial parallel workspace invocation hit the CLI unit test's existing
`HOME`/`HEDDLE_HOME` race in
`remote_name_resolves_to_the_hosted_spool_path`. Completed coverage follows
`.github/workflows/rust-tests.yml`'s serial CLI unit command. An initial CLI
integration invocation without `ci` was refused before executing tests because
`ci_run_local` requires that feature; the completed run includes it.

Applied surfaces: no verb/help/clap or output change; human/agent contracts and
Git interop are unchanged; immutable native originals and full public evidence
remain exportable; no wire field or API pin changes; replay, root replacement,
revocation and clock rollback remain observable through existing APIs and the
new snapshot. Capability-verifier is untouched, so the conditional WASM build
does not apply.
