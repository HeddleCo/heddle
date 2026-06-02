# heddle#413 - ref stale-lock substrate

**Status:** spike decision doc. No production code lands in this issue.

**Decision:** implement #372 with **Option A: OS advisory locks** for the
file-backed refs mutex, using the workspace's existing `fs2` dependency. Keep
`refs/LOCK` as the lock path, but make it a stable advisory-lock file whose file
descriptor is held by `RefsLock`; do not delete it on drop. Do not extend the
current timestamp reaper.

The important distinction from the existing reservation liveness helper is
lifetime. `crates/objects/src/store/liveness.rs` says a `flock` does not help
`heddle agent reserve` because that one-shot CLI process exits immediately after
recording the reservation (`liveness.rs:4-8`). The refs lock is the opposite:
`lock_refs` returns a guard and every write/reconcile critical section executes
while that guard is alive (`crates/refs/src/refs/refs_manager.rs:212-215`,
`crates/refs/src/refs/refs_transactions.rs:385-401`). For refs, kernel release
on descriptor close or process death directly matches the ownership lifetime.

---

## Current facts

The file-backed ref mutex is `refs/LOCK`
(`crates/refs/src/refs/refs_storage.rs:47-49`). It is currently a lockfile by
existence: `lock_refs` creates the refs directory, records the current PID, and
tries `OpenOptions::create_new(true)` on `refs/LOCK`
(`refs_storage.rs:153-181`). The lock body is exactly `"<pid> <unix_ts>"`, and
`parse_lock_content` only parses those two fields (`refs_storage.rs:179-181`,
`:201-205`). `RefsLock::drop` removes the lock path (`refs_storage.rs:27-39`).

The stale-lock reaper is age-only. On every acquire loop, if `refs/LOCK` can be
read and parsed, the code removes it when the owner PID is not the current PID
and `now - ts > STALE_LOCK_TIMEOUT_SECS` (`refs_storage.rs:168-173`). The
timeout is 300 seconds (`refs_storage.rs:22`). There is no call to
`is_owner_alive`, no hostname, no boot id, and no heartbeat in the refs lock body
or acquire loop (`refs_storage.rs:168-181`, `:201-205`).

Ref writes rely on this mutex for linearization. The write chokepoint takes the
refs lock first, materializes committed tail state, and then runs the caller's
body under that one lock (`crates/refs/src/refs/refs_manager.rs:186-215`).
`publish_ref_plans` stages new ref contents into temp files and publishes them
with `std::fs::rename` while holding `&RefsLock`
(`crates/refs/src/refs/refs_transactions.rs:380-401`). Atomic file replacement
elsewhere in the repo also uses temp-write, file sync, rename, and parent-dir
sync on Unix (`crates/objects/src/fs_atomic.rs:203-231`, `:373-405`).

The existing process-liveness helper records a stronger owner identity than refs
currently do. It has `Liveness::{Alive, Dead, Unknown}`, with `Unknown` meant to
avoid reaping live owners when fields are missing
(`crates/objects/src/store/liveness.rs:16-24`). On Linux and macOS it can derive
a current boot id (`liveness.rs:27-65`), and on Unix it treats `kill(pid, 0)`
`ESRCH` as dead while treating other errors, including `EPERM`, as alive
(`liveness.rs:67-84`). On Windows it currently returns alive for every PID
because no Win32 process query is implemented (`liveness.rs:86-91`).
`is_owner_alive` combines PID and boot id, returning dead on PID death or boot-id
mismatch and alive otherwise (`liveness.rs:94-109`).

Heddle shares a ref root across sibling materialized checkouts, not through an
explicit distributed-ref protocol. In objectstore-pointer worktrees, the shared
ref root is the canonical shared `.heddle` directory, while `local_head` is
per-worktree (`refs_storage.rs:83-87`). `Repository::open` resolves the absolute
objectstore pointer, validates it contains `objects/`, then builds
`RefManager::new(&shared_galeed_dir).with_local_head(local_head_path)`
(`crates/repo/src/repository.rs:680-703`). `Repository::init_worktree` writes
the pointer as an absolute path (`repository.rs:1658-1673`), and the CLI
materialized-worktree path does the same from `repo.heddle_dir()`
(`crates/cli/src/cli/commands/worktree_cmd/helpers.rs:261-268`). The refs tests
model two sibling checkouts sharing one ref root with distinct local HEAD files
(`crates/refs/src/refs/refs_tests.rs:482-505`). I found no code path or doc
claim that a single file-backed refs directory is intended as a cross-host
coordination domain.

The workspace already has `fs2 = "0.4.3"` (`Cargo.toml:53`), and the objects
crate already uses `fs2::FileExt` for repository read/write locks
(`crates/objects/src/lock.rs:10`, `:57-68`). That helper keeps the lock file
descriptor inside the guard and unlocks on drop (`objects/src/lock.rs:23-40`).

---

## Failure mode

The current age-only reap can break mutual exclusion even when the original
owner is alive and correctly inside the critical section:

1. T0: process A enters a long ref mutation. `lock_refs` creates `refs/LOCK`
   with content like `1234 1700000000`, using `create_new(true)`, and returns a
   `RefsLock` guard (`refs_storage.rs:177-189`).
2. T0..T+300s: A legitimately keeps running under that guard. The critical
   section can include reconciliation before the caller body
   (`refs_manager.rs:186-215`) and temp-file/rename publication
   (`refs_transactions.rs:380-401`). The current lock body is not refreshed.
3. T+301s: process B calls `lock_refs`. It reads the lock body, sees `pid !=
   current_pid`, computes `now - ts > STALE_LOCK_TIMEOUT_SECS`, and removes
   `refs/LOCK` (`refs_storage.rs:168-173`).
4. B then immediately succeeds at `create_new(true)` for a new `refs/LOCK` and
   receives its own `RefsLock` (`refs_storage.rs:177-189`).
5. A still believes it owns mutual exclusion. A and B can both validate, append,
   materialize, rename refs, update packed refs, or rebuild indexes under
   different logical locks. The chokepoint invariant that concurrent publishes
   serialize under one held refs lock is false (`refs_manager.rs:143-153`,
   `:186-215`).
6. When A drops its guard, `RefsLock::drop` removes `refs/LOCK`
   (`refs_storage.rs:31-39`). If that path now names B's lockfile, A can delete
   B's lock out from under it too.

This is not a timeout tuning problem. Any finite age-only threshold can be
exceeded by a slow but live operation.

---

## Option A - OS advisory locks

Use `refs/LOCK` as a persistent lock file and acquire an exclusive advisory lock
on its open file descriptor. `RefsLock` should hold the `File` for the duration
of the critical section. Release happens by `FileExt::unlock()` on drop and, as
the key property, by the OS closing the descriptor when the process exits or is
killed.

The implementation shape should mirror the existing repo-lock precedent in
`crates/objects/src/lock.rs`: open or create a stable lock file, call
`try_lock_exclusive` in the existing wait loop, keep the `File` in the guard, and
unlock on drop (`objects/src/lock.rs:57-68`, `:81-88`, `:98-100`). `refs` would
add `fs2.workspace = true` to `crates/refs/Cargo.toml`; `fs2` is already in the
workspace and used by `objects` (`Cargo.toml:53`, `crates/objects/src/lock.rs:10`).

Semantics:

- **No stale lock after process death.** `kill -9`, panic abort, or normal exit
  closes the descriptor, so another process can acquire without deleting a
  possibly-live owner's lockfile.
- **No age-based ownership guess.** `STALE_LOCK_TIMEOUT_SECS`,
  `parse_lock_content`, and the `remove_file` reaper become unnecessary for refs.
- **The lock path must not be removed on drop.** On Unix, deleting a locked file
  can split the lock domain: the original process keeps a lock on the unlinked
  inode while another process creates and locks a new `refs/LOCK` path. The
  stable inode is part of the design.
- **Current temp/rename ref publication stays the same.** The advisory lock is a
  mutex around the existing critical section. It does not replace temp files,
  `rename`, `sync_directory`, packed-ref removal, rollback, or summary-index
  rebuilds (`refs_transactions.rs:380-445`).

Cross-platform behavior:

- Linux and macOS: `fs2` routes through platform file-locking primitives. Locks
  are advisory, so correctness depends on all Heddle file-backed ref writers
  using the same lock path. The current raw writer chokepoint already makes that
  centralization testable (`refs_manager.rs:204-215`).
- Windows: `fs2` provides exclusive file locking through Windows APIs. This is
  materially better than Option B with the current liveness helper, because
  `process_alive` currently defaults every PID to alive on non-Unix platforms
  (`liveness.rs:86-91`).
- Same-process reentrancy: advisory locks are not a recursive mutex. Preserve
  the current one-lock critical-section shape and avoid taking a second refs lock
  while holding one. Existing code intentionally passes `&RefsLock` through the
  under-lock paths instead of reacquiring (`refs_transactions.rs:103-109`,
  `refs_manager.rs:212-215`).

Network filesystem caveat:

Advisory locking semantics vary across NFS and other network filesystems
depending on protocol version, mount options, and lock daemon behavior. That is
the main downside of Option A. The checked Heddle code supports sibling
worktrees by pointing multiple local checkouts at one shared `.heddle` path
(`repository.rs:680-703`, `refs_tests.rs:482-505`); it does not establish a
cross-host file-backed refs contract. If cross-host shared refs become a product
requirement, the right substrate is probably the hosted/Postgres backend or a
real lease service, not PID-based local liveness.

---

## Option B - keep lockfile and add liveness

Keep the current `create_new(true)` lockfile protocol and make stale removal
conditional on an owner-liveness record instead of age alone.

A minimally sound owner record would need at least:

```text
version pid host boot_id acquired_at refreshed_at
```

Acquire/reap logic would become:

1. If `refs/LOCK` is absent, create it with `create_new(true)`, write the owner
   record, `sync_all`, and hold the `RefsLock`.
2. If it exists and is younger than the stale threshold by `refreshed_at`, wait.
3. If it exists and is stale, inspect the owner:
   - same host: call `is_owner_alive(Some(pid), boot_id.as_deref())`;
   - `Dead`: remove the lock and retry create;
   - `Alive` or `Unknown`: do not reap;
   - different host: do not interpret the PID. Either fail closed or require a
     separate cross-host lease policy.
4. While the guard is held, refresh `refreshed_at` often enough that a live long
   operation does not look stale. This can be a heartbeat thread owned by
   `RefsLock`, or explicit refresh calls around known long phases.

This option reuses useful existing code. The helper already models `Alive`,
`Dead`, and `Unknown` with a fail-closed `Unknown` state (`liveness.rs:16-24`),
checks PID existence on Unix (`liveness.rs:67-84`), and compares boot ids where
available (`liveness.rs:27-65`, `:94-109`). It also lines up with the current
lockfile create/delete scheme (`refs_storage.rs:177-189`, `:31-39`).

Its costs are high for refs:

- **Crash safety is delayed and platform-sensitive.** On Unix, a dead owner can
  be reaped after the stale threshold. On Windows, the current helper always
  reports alive (`liveness.rs:86-91`), so #372 would need a real Win32 process
  liveness implementation or Windows stale locks would fail closed forever.
- **Heartbeat correctness becomes part of the mutex.** A missed refresh, clock
  skew, parsing failure, host-name change, or boot-id unknown can turn into
  either a false reap or a permanent lock.
- **Cross-host remains unresolved.** A PID from another host is meaningless. The
  code supports local sibling checkout sharing, but I found no current promise
  that file-backed refs are coordinated across hosts (`repository.rs:680-703`,
  `refs_tests.rs:482-505`).
- **The old drop hazard remains unless carefully changed.** If a process reaps
  and recreates `refs/LOCK` while a live owner is still around because of any
  liveness bug, the old owner's `Drop` can remove the new owner's path
  (`refs_storage.rs:31-39`).

Option B is the right pattern for records whose ownership outlives the process
that wrote them. That is exactly why `liveness.rs` exists for reservations
(`liveness.rs:4-14`). The refs lock is not such a record; it is a process-held
critical-section guard.

---

## Recommendation and rationale

Choose **Option A: OS advisory locks via `fs2`**.

Why:

- **Crash safety:** best. Process death releases the kernel lock immediately;
  there is no stale-lock file to classify and no 300-second vulnerability window.
- **Long operation safety:** best. A live holder cannot be reaped by wall-clock
  age because there is no age reaper.
- **Cross-platform:** better than Option B in this codebase. `fs2` is already a
  workspace dependency and already used for repository locks (`Cargo.toml:53`,
  `objects/src/lock.rs:57-68`). Option B would need extra Windows liveness work
  because the current helper defaults to alive on non-Unix (`liveness.rs:86-91`).
- **Complexity:** lower. The implementation replaces PID/timestamp parsing and
  stale deletion with a descriptor-held guard. It does not add a heartbeat, owner
  schema, host identity, boot-id migration, or liveness-dependent reap policy.
- **Reuse of existing code:** good. Reuse the `fs2::FileExt` pattern already in
  `objects::lock`; do not force the reservation liveness helper into a mutex
  lifetime it was not designed for.

The tradeoff is NFS/network-fs behavior. Given the checked code's current local
sibling-worktree sharing model and lack of a cross-host file-backed refs
contract, that caveat is acceptable for #372. If Heddle later needs shared refs
on network filesystems across hosts, revisit the substrate explicitly and prefer
a hosted/Postgres or lease-backed backend.

---

## Implementation sketch for #372

Files:

- `crates/refs/Cargo.toml`: add `fs2.workspace = true`.
- `crates/refs/src/refs/refs_storage.rs`: replace the lockfile-existence mutex
  with a descriptor-held advisory lock.
- Tests in `crates/refs/src/refs/refs_storage.rs` or a small refs integration
  test module.

Concrete changes:

1. Change `RefsLock` to hold the lock file:

   ```rust
   pub(super) struct RefsLock {
       file: std::fs::File,
       path: PathBuf,
   }
   ```

   `path` is useful for diagnostics only. `Drop` should call `self.file.unlock()`
   and must not remove `self.path`.

2. Change `lock_refs`:

   - keep `std::fs::create_dir_all(self.refs_dir())`;
   - open `self.lock_path()` with `OpenOptions::new().read(true).write(true).create(true).open(...)`;
   - loop until `MAX_LOCK_WAIT_SECS` using `try_lock_exclusive`;
   - on success return `RefsLock { file, path }`;
   - on contention sleep with the existing jitter/backoff;
   - preserve the current timeout error text or a close equivalent.

3. Remove refs stale-lock parsing and reaping:

   - delete or stop using `STALE_LOCK_TIMEOUT_SECS`;
   - delete `parse_lock_content` unless another test still needs it;
   - remove the `read_string` -> `parse_lock_content` -> `remove_file` block from
     `lock_refs` (`refs_storage.rs:168-173`);
   - remove the current stale-removal test that asserts an old fake PID file is
     deleted/acquired (`refs_storage.rs:398-409`), replacing it with advisory-lock
     tests below.

4. Leave ref publication call sites alone:

   - `write_chokepoint` continues to call `let lock = self.lock_refs()?` and pass
     `&lock` through materialization and body (`refs_manager.rs:212-215`);
   - `reconciled_load` continues to take the lock before under-lock fold and
     materialization (`refs_manager.rs:341-370`);
   - `publish_ref_plans` continues to require `&RefsLock` and publish via temp
     files and `rename` (`refs_transactions.rs:385-401`).

5. `is_owner_alive` has no refs call site in this design. Leave
   `crates/objects/src/store/liveness.rs` scoped to reservation reaping.

Test plan:

- **Basic acquire/release:** acquire `lock_refs`, assert a second acquisition on
  another manager/path handle cannot get the advisory lock while the first guard
  is alive, drop the first guard, then assert the second can acquire. Prefer a
  nonblocking test helper so the test does not wait 10 seconds.
- **Long op plus concurrent acquirer:** hold the lock across a sleep longer than
  a test-injected stale threshold or with an old pre-existing `refs/LOCK` body.
  The second acquirer must not unlink or replace the path and must not enter the
  critical section until the first guard drops. This directly covers the #372
  race.
- **Dead owner reclaim:** spawn a child process that opens the same refs lock and
  exits without an explicit unlock, then assert the parent can acquire. On Unix,
  also cover forced termination if practical. On Windows, process exit should
  release the lock as well.
- **Pre-existing legacy lockfile:** write arbitrary old PID/timestamp content to
  `refs/LOCK` before acquiring. The new code should treat it as a lock file inode
  and acquire the advisory lock if no process holds it.
- **No unlink on drop:** acquire and drop the lock, then assert `refs/LOCK` still
  exists. This guards against reintroducing the Unix inode-split bug.
- **Serialization through current chokepoint:** keep or adapt existing
  concurrent ref write tests so two `commit_and_publish` callers still serialize
  under the one lock and do not append/publish out of order
  (`refs_manager.rs:143-153`, `:186-215`).

---

## Open questions and risks

- Should file-backed refs explicitly document network filesystems as unsupported
  for cross-host concurrent writers? This spike recommends doing so unless a
  product requirement says otherwise.
- Should #372 expose a small test-only nonblocking refs-lock helper or injectable
  timeout to avoid 10-second contention tests?
- Should `objects::lock` grow a reusable exclusive-lock primitive, or should
  `refs_storage.rs` use `fs2::FileExt` directly? Direct use is more surgical;
  shared code is only worth it if the error mapping stays clear.
- `fs2::FileExt` locks are advisory. Any future raw file-backed ref writer that
  bypasses `lock_refs` can still corrupt state. The existing no-bypass
  conformance around private raw writers should remain part of the test floor
  (`refs_manager.rs:204-211`).
- Same-process nested refs locks can self-deadlock. Keep passing `&RefsLock`
  through under-lock helpers instead of reacquiring inside them.
