# HYBRID Part 1b: core seams for #1963

Part of #1961; installation seams consumed by #1963. This revision resolves
PR #1966's F1–F5 security review. Integration Part 3 (`477e6049`) was included
with a plain merge (`9eb561fe`). The API remains integration's alpha.21 pin,
`05c4d08c4e7120c2532a2b2a6c608b12e58acc4b`; Part 2 owns the repin. The three
published core fixture copies remain unchanged.

## Public signatures and callback contract

```rust
ThreadReplica::install_hybrid_import(
    directory: &Path,
    trust: &HostedTrust<impl Clock>,
    bundle_bytes: &[u8],
    native_records: &[wire::SignedRecord],
    authority: &impl AcceptedAuthority,
    store: &impl ObjectStore,
    before_commit: impl FnOnce(&mut InstallArtifacts<'_>) -> Result<()>,
) -> Result<Vec<ThreadReplica>>;

// repo::thread_replication::install_artifacts
pub struct InstallArtifacts<'a> { /* private borrowed writer */ }
impl InstallArtifacts<'_> {
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

// heddle_fs_prims::fs_atomic
pub fn durable_rename(source: &Path, destination: &Path) -> std::io::Result<()>;
// heddle_fs_prims::directory (also re-exported by objects)
impl Directory {
    pub fn durable_rename(&self, source: &OsStr, destination: &OsStr)
        -> std::io::Result<()>;
}
```

Part 2 must stage receives in an isolated object store, then publish actual
packs, pins and sidecars through this writer. `destination` is relative to the
selected `heddle_dir`; destination parents must already exist. `staged` names a
trusted staging input. Core owns both the SQL transaction and private journal.
The writer has no constructor or `Default`; moving or forgetting the borrowed
facade cannot move the journal. The `std::mem::take` compile-fail doctest ran.
Arbitrary filesystem operations in trusted callback code are outside this seam.

## F1: durable, bounded installation recovery

One repository intent names the installation UUID, relative destinations,
parent identities, backup/preparation state and deterministic new/restore names.
The bound is 1,024 destinations, a 1 MiB intent and 4,096 bytes per UTF-8 path.
Backups stream through file handles rather than accumulating pack bytes in RAM.
There is no transaction service or general transaction framework.

For each first write, persist intent before preparing a backup or publishing.
Copy the original from its no-follow opened handle; preserve permissions, flush
file data, durably publish the backup, then persist the prepared bit. Write and
flush the new file before durably publishing its directory entry. Repeated
writes retain the first backup. Core inserts the installation UUID into
`hosted_installation` in the same SQL transaction as replicas, trust floors,
proofs and job associations. Parent binding checks and marker insertion precede
the final receiver-clock, witness-lifetime and current-disclosure checks so
filesystem I/O cannot delay that final authority sample. Local metadata remains
SQLite WAL with FULL sync;
the marker schema migrates v5 to v6.

The selected repository's existing exclusive `locks/repo.lock` serializes
installations, recovery and native/trust readers, across processes and accepted
authorities. Open recovers before interpreting mutable repository config or
exposing its object store. Initialization creates and holds this same lock so
a later observation does not first create repository state. Trust/native connections retain serialization while
reading. Reentrant trusted access during an active callback rejects.

On reopen, read the SQL marker: a committed installation keeps its published
files; an uncommitted installation restores originals and retires new files.
Restore hard-links the durable backup to a temporary name, then publishes it;
the backup is never consumed by an attempted restoration. A durable `done` bit
precedes backup cleanup. Retire each backup/temp to a tombstone durably before
unlinking; retire the active intent only after all undo retirement succeeds.
A crash during recovery replays safely, and a lost Windows tombstone unlink is
harmless. Parent identity mismatch or incomplete recovery refuses trusted use.

Unix capability publication uses `renameat` followed by `fsync` on the held
directory, independent of clone durability suppression. Windows flushes file
handles and calls `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)` for publication,
restore and intent/tombstone retirement. The existing Windows `sync_directory`
no-op is explicitly documented as providing no durability; installation does
not use it as a barrier. Microsoft's
[MoveFileExW contract](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-movefileexw)
specifies write-through completion before return.

`process_exit_recovery_is_repeatable_at_every_journal_step` runs 33 installation
crash scenarios plus 17 scenarios crashing recovery again (67 child exits).
It covers every occurrence of intent, backup, preparation, file flush,
publication, marker, precommit, commit, done, rollback and cleanup checkpoints.
`hybrid_installation_process_exit_reconciles_sql_and_artifacts` additionally
exits the real public installer at six publication/commit/cleanup steps and
reopens, checking artifacts and durable SQL together.
`hybrid_crash_recovers_config_before_repository_readers` crashes after
publishing invalid config and proves ordinary repository open restores it first.

## F2: unwind and SQL destruction

Core catches callback and post-callback validation panics while exclusive
serialization is still held. It explicitly rolls SQL back and destroys the
connection before restoring artifacts. It then releases the clock mutex and
repository lock before resuming the original panic, avoiding mutex poisoning.
Cleanup is committed only after successful SQL commit; ambiguous commit failure
closes SQL and consults the same marker-based recovery protocol. Aborting panics
use F1 on the next open. Tests cover callback and final-hook panics, subsequent
trusted use, final rejection and an actual SQLite commit-hook rejection.

## F3: retryable rollback failures

Undo attempts every independent entry, retaining the manifest and durable
backups when any rename or unlink fails. A failed restore cannot lose the
backup through `PersistError` or a tempfile destructor: core owns ordinary,
explicitly named files and never consumes the backup. Recovery retries under
the same lock and protocol. Callback/authority rejection remains the returned
error; cleanup failure is logged separately. Snapshot, ordinary open and the
next installer reject while the injected failure persists, and resume after
recovery succeeds. Rename/unlink failures are injected at all four rollback
positions, including mixed preexisting/new files and independent restoration.

## F4: exact landing dependency admissions

A purpose-4 landing witness admits only its execution original. Its source and
review originals require their own verified purpose-2 first-admission evidence.
The dependency selection/coverage gate runs before the artifact callback; each
original's exact statement and signature is retained. Tests use valid published
originals with separately signed first admissions, cover a valid landing,
remove each dependency's evidence independently, and land after source/review
were already admitted with both statement orders. Existing exact sidecars are
checked after the landing; fixture bytes are unchanged.

## F5: selected repository containment

Absolute destinations, traversal, reserved transaction/lock names and Windows
aliases/alternate streams reject. Unix traverses single child components with
`openat(O_NOFOLLOW | O_DIRECTORY)` and creates, links, renames and removes through
held directory handles. Replacement cannot redirect publication or rollback;
the parent identity is checked again before SQL commit and during recovery.
The selected root capability remains held for the entire serialized operation
and is cloned into the journal, never reopened after trust verification. Tests
cover static traversal, a substituted symlink parent, root replacement between
serialization and journal creation, and parent replacement after resolution, including unchanged outside bytes
and restoration in the original held parent.

Windows resolves directory children using handle-relative `NtCreateFile` with
`OBJ_DONT_REPARSE`, rejects reparse points, and holds all ancestors without delete
sharing. An undeletable, handle-relative, delete-on-close pin file keeps the leaf
nonempty; each held child keeps its ancestor nonempty. This matters because
attribute-only writes bypass sharing restrictions, while Microsoft's
[FSCTL_SET_REPARSE_POINT protocol](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-fsa/4aeefef8-92c3-4abc-af7a-a610caf8a165)
requires nonempty directories to reject reparse substitution. Path-based
write-through publication therefore cannot be redirected through a renamed or
retagged ancestor. Windows tests cover denied parent replacement, durable
publish/restore, rejection of an existing junction, and an attribute-only
junction attack with a successful unpinned control. These operations need no
administrator or symlink privilege.

## Executed fail-then-pass proofs

Logs and the mutation script are retained in `/tmp/1966-security-evidence`.
Each named test was executed after removing its production guard, then executed
again after restoring the source bytes. Every removed run exited 101 with
`0 passed; 1 failed`; every restored run exited 0 with `1 passed; 0 failed`.

| Finding | Named regression | Guard removed |
| --- | --- | --- |
| F1 recovery | `hybrid_installation_process_exit_reconciles_sql_and_artifacts` | Open-time recovery |
| F1 marker | `hybrid_installation_process_exit_reconciles_sql_and_artifacts` | Same-SQL-transaction installation marker |
| F2 | `hybrid_callback_and_post_callback_panics_restore_before_unwinding` | Filesystem undo for the unwind outcome |
| F3 independent undo | `failed_rollback_keeps_independent_entries_and_durable_undo` | Continuing after one failed rollback entry |
| F3 original error | `hybrid_rollback_io_preserves_original_rejection_and_blocks_trusted_use` | Preservation of the original rejection |
| F4 | `hybrid_landing_requires_each_dependency_first_admission_before_callback` | Execution-only coverage for a landing witness |
| F5 root binding | `repository_root_replacement_cannot_redirect_journal_begin` | Cloning the selected root capability instead of reopening its path |
| F5 beneath | `parent_symlink_substitution_cannot_escape_repository` | Capability no-follow open |

Representative executed output from each pair:

```text
removed:  test result: FAILED. 0 passed; 1 failed; 0 ignored
restored: test result: ok. 1 passed; 0 failed; 0 ignored
```

The earlier Part 1b guard-removal evidence (selected branch, uncovered original,
read-only snapshot and clock floor) remains in `/tmp/1961p1b-evidence`. Its
regressions are also covered by this revision's workspace run.

## Gates

Workspace coverage uses a fresh `HEDDLE_HOME` and CI's partitioned CLI execution:

```sh
export HEDDLE_HOME=$(mktemp -d /tmp/1966-workspace-home.XXXXXX)
cargo test --workspace --exclude heddle-cli --locked -- --test-threads=8
cargo test --locked -p heddle-cli --lib --bins -- --test-threads=1
cargo test --locked -p heddle-cli --features ci --test '*' -- --test-threads=8
cargo test --locked -p heddle-cli --doc
cargo clippy --workspace --all-targets --locked -- -D warnings -D dead-code
cargo clippy --workspace --all-targets --locked --features heddle-cli/ci -- -D warnings -D dead-code
```

| Gate | Executed result |
| --- | --- |
| Fresh-home non-CLI workspace tests, including docs | 4,487 passed; 0 failed; 24 ignored |
| Fresh-home serialized CLI library/binary units | 504 passed; 0 failed; 0 ignored |
| Fresh-home CI-feature CLI integration | 1,578 passed; 0 failed; 79 existing ignores; all 54 binaries across two partitions |
| CLI docs | Exit 0; no CLI doctests (the borrowed-facade doctest ran in repo) |
| Journal regressions | 6 passed; 0 failed; 1 child entry ignored |
| Hosted-trust regressions | 26 passed; 0 failed; 1 child entry ignored |
| fs-prims Linux library | 59 passed; 0 failed; 1 ignored |
| Workspace Clippy, all targets, warnings/dead-code denied | Exit 0 |
| Workspace CI-feature Clippy, all targets, warnings/dead-code denied | Exit 0 |
| Nightly edition-2024 rustfmt, all 23 touched Rust files | Exit 0 |
| Windows exact backend/test modules, MSVC-target check and Clippy | Exit 0 (isolated harness) |

Selected executed cargo output (the full workspace includes the three changed
library crates below):

```text
heddle-fs-prims: test result: ok. 59 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.56s
objects: test result: ok. 313 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 16.80s
repo: test result: ok. 1012 passed; 0 failed; 5 ignored; 0 measured; 0 filtered out; finished in 228.57s
journal: test result: ok. 6 passed; 0 failed; 1 ignored; 0 measured; 953 filtered out; finished in 47.79s
hosted trust: test result: ok. 26 passed; 0 failed; 1 ignored; 0 measured; 933 filtered out; finished in 28.27s
CLI library: test result: ok. 499 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 207.77s
CLI binary: test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s
CLI docs: test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```


Two CLI unit attempts exposed existing test setups that held the repository
lock while awaiting workers that first opened the repository. Open now must
acquire exclusive recovery serialization. The overlay test moves its worker
barrier before open; the land ownership test opens its contender before taking
the first land lock. Both targeted regressions passed. The canceled, deadlocked
test processes were stopped, and the complete fresh-home gate was restarted
after these setups and final authority ordering were fixed. No lock requirement
was weakened.

Windows execution is delegated to `.github/workflows/installation-windows.yml`:
non-privileged directory/publish tests and serial journal crash/recovery tests
with a fresh home. Locally, the exact `fs_atomic.rs` and `directory.rs` modules
and their Windows tests passed MSVC-target `cargo check --tests` and Clippy in
an isolated harness. A full Windows crate check was attempted but blocked by
the missing MSVC `ml64.exe` assembler used by blake3. Windows binaries and
Windows crash tests were not run on this Linux host; macOS was not run locally.
Runtime process-exit tests establish restart behavior, not a physical power-cut
experiment; file/entry durability additionally depends on the platform flush
contracts described above.

Applied surfaces: no verb/help/clap or human/agent output change; Git import,
export and projection stay with sley; immutable native originals and full
public evidence remain exportable; no wire field or API-pin change; reverse
states include durable rollback, retry, cleanup and open-time recovery.
A CLI unit attempt also found the source-scanning identity audit matching
a crash-test variable named `HEDDLE_INSTALL_CRASH_CONFIG`. Renaming it to
`HEDDLE_INSTALL_CRASH_INVALID_TOML` describes the injected invalid config without
adding a test-only variable to production credential scrubbing. The hosted-trust
regressions were rerun after the rename.

The first CI-feature CLI integration attempt also caught two observation
regressions: an initialized Git overlay lacked `locks/repo.lock`, so open-time
serialization created it during a read. Initializers now acquire the same
installation lock before exposing storage; observation remains byte-preserving
when no recovery is pending. The existing initialized/unbound history tests
were left intact and rerun after this production fix: `102 passed; 0 failed;
0 ignored`. The before-fix integration log and named failure reproduction are
retained alongside the passing matrix log.

The final integration run found another old lock-order assumption in
`list_cannot_reap_a_writer_during_its_capture`: it stalled the capture child on
the repository lock before native open could reach its checkout mutation lock.
The replacement `list_cannot_reap_an_authenticated_checkout_writer` holds the
production authenticated capture guard, proves the lease is actually expired,
runs the real `agent list` in another process, verifies subsequent capture still
works, and proves expiry is reapable after guard release. Its named run passed.
Only this test fixture changed after the production gates passed. The 36 passing
integration binaries are retained; the affected binary and all 17 unrun binaries
complete in a fresh-home partition. Failed-attempt logs are excluded from the
reported passing counts. The final coverage audit compares the union of
passing binary results against all 54 CLI test targets from Cargo metadata.
The fresh-home completion command was:

```sh
export HEDDLE_HOME=$(mktemp -d /tmp/1966-final-integration-home.XXXXXX)
cargo test --locked -p heddle-cli --features ci --test multi_agent_worktrees --test netd_daemon --test onboarding_contract --test op_id_coverage --test performance --test production_features --test provenance_verify --test render_lint --test roundtrip_fidelity --test semantic_diff_integration --test semantic_refs_cli --test semantic_symbol_diff_cli --test stack_rebase --test state_management --test tier_coverage --test typed_error_lint --test undo_git_rollback_race --test whoami_capture_actor -- --test-threads=8
```

Changes touch repo and fs-prims, the objects re-export, two CLI concurrency
test setups and one writer-lease integration fixture, this evidence document
and Windows CI. Capability-verifier is
unchanged, so its conditional WASM gate does not apply.
