# Clonefile-backed Lightweight Threads — Design Note

**Status:** Approved, day-one default on reflink-capable hosts.
Prototype validated against the heddle workspace (48.68 s `cargo build`
against a clonefile-materialized tree vs 48.94 s vanilla baseline — par
within noise — plus 91 ms upfront materialize for 643 files). CLI
surface in flight; virtualized mounts stay supported as the opt-in for
very-large-repo and remote-backed-CAS workflows.

**Owner:** mount + materialization (`crates/mount/`, `crates/repo/`).

**Trigger:** end-to-end FSKit `cargo build` lands at +2.4% vs vanilla
after the cache + zero-copy + daemon-pool work in `a5db30d`. That
overhead is structurally bounded by the kernel↔fskitd↔.appex XPC chain;
the only way past it is to take FSKit out of the hot path. This note
describes the system that does.

## Goals

1. **Native-speed reads** for the agent's working tree. A `cargo build`
   inside a lightweight thread should run at the same wall-clock as a
   `cargo build` against a plain checkout — no FSKit, no FUSE, no
   userspace FS callbacks in the hot path.
2. **Sub-100 ms thread switch.** "Switching threads" should feel like
   `cd`, not like clone. Measured: 91 ms for the heddle workspace; we
   expect this to stay under 200 ms for repos up to ~10k files.
3. **Bounded disk per thread.** Disk usage is `(metadata) + (bytes the
   agent modified)`, not `(full repo) × (threads)`. This is the
   storage win clonefile makes possible.
4. **Same data model.** Threads still resolve to states, states to
   trees, trees to blobs. The CAS is unchanged. Only the
   *materialization* of a thread changes.

## Non-goals

- Replacing the virtualized (FSKit/FUSE/ProjFS) mount entirely. That
  path stays for **very large repos** where eager materialize is too
  expensive, and for **remote-backed mounts** where blobs don't all
  live on local disk.
- Real-time write tracking. Capture is explicit (the user runs
  `heddle capture` or it's invoked by another command), modelled on
  git's index. We do not intercept writes in the FS layer.
- Cross-platform parity on day one. macOS + APFS is the first target;
  Linux btrfs/XFS-reflinks follows immediately (same primitive,
  different syscall). Windows is intentionally last and falls back to
  the existing ProjFS path.

## Background

Today's lightweight-thread story on macOS:

```
agent → cargo build → kernel read(2) → fskitd → HeddleFSModule.appex → Rust core → blob bytes
```

Each `read` on a file roundtrips through Apple's userspace-FS chain.
For a 643-file `cargo build` we measured **~5 ms × 643 callbacks ≈ 3 s
of overhead** on top of the actual compile work. The Rust core itself
already serves hot-cache reads at memcpy speed (faster than vanilla
`std::fs::read`); the remaining gap is structurally Apple's IPC.

But the *content* of the thread is just bytes the CAS already knows
how to produce. The cache work this session showed we can serve those
bytes from memory at vanilla speed. The materialize work in
`crates/repo/src/repository_materialization.rs` already knows how to
write them to disk **without copying their bytes** — `clonefile(2)`
shares APFS extents with the canonical loose-uncompressed store on the
same volume.

So: instead of teaching FSKit to be faster, write the bytes to disk
once, point the agent at the directory, and let APFS do what it
already does.

## High-level architecture

```
                    ┌──────────────────────────────────────┐
                    │           CAS (.heddle/objects)      │
                    │  loose-uncompressed canonical blobs  │
                    └──────────────┬───────────────────────┘
                                   │ clonefile(2)
                                   ▼
   ┌──────────────────────────────────────────────────────────┐
   │     Thread root: <repo>-heddle-mounts/<thread>/          │
   │  Real APFS directory. Reads pass straight to the kernel  │
   │  page cache. Writes diverge via APFS copy-on-write.      │
   └──────────────────────────────────────────────────────────┘
                                   │
                                   │ heddle capture
                                   ▼
                       walk + hash dirty files
                                   │
                                   ▼
                          new Tree + new State
```

The thread root is a normal directory. Every file in it is either a
clonefile of the canonical loose blob (zero new disk usage) or — after
the agent edits it — a copy-on-write divergent copy (new disk usage
proportional to the changed 4 KiB blocks).

## Detailed design

### Materialization (eager)

On `heddle start <name>`:

1. Resolve `<name>` to a state via `RefManager`.
2. Resolve the state to a tree via `ObjectStore::get_state`.
3. Compute the thread-root path
   (`<workspace_parent>/.<repo_name>-heddle-mounts/<sanitized_name>/`,
   matching the existing convention for FUSE/FSKit mounts).
4. Call `Repository::materialize_tree(&tree, &thread_root)`.
   - The existing materializer walks the tree, creates directories,
     and for each blob:
     - Tries the loose-uncompressed canonical path
       (`ObjectStore::loose_blob_path(hash)`).
     - On miss, `promote_to_loose_uncompressed(hash)` — pays one
       decompress + atomic write, then retries.
     - Calls `clonefile_or_copy(canonical, thread_path)` from
       `objects::fs_clone`.
5. Write a `manifest.toml` at
   `<repo>/.heddle/threads/<name>/manifest.toml` recording, per file:
   - Relative path
   - Content hash
   - inode, mtime nanos, ctime nanos at materialize time
   - Unix mode bits
   This is the **stat-cache** that makes the capture scan fast (see
   below).
6. Update the thread ref to point at the resolved state (idempotent;
   `thread start` from an existing thread keeps the head).

This is purely additive; the existing CAS, refs, and oplog are
untouched.

### Capture (scan-on-capture)

On `heddle capture` from inside a thread root:

1. Load `manifest.toml`.
2. Walk the thread root with the `ignore` crate (respects `.gitignore`
   plus a `.heddleignore` for thread-only excludes — `target/`, build
   caches, etc.).
3. For each file in the walk:
   - `stat` it.
   - If `(inode, mtime_nanos, ctime_nanos, mode)` matches the manifest
     entry, reuse the manifest's content hash — file is unchanged.
     (APFS clonefile preserves the destination's inode; the manifest's
     `(inode, mtime)` is a reliable "did anyone touch this file?"
     stat-cache, same as git's index.)
   - Otherwise: read + hash + `ObjectStore::put_blob`. Update the
     in-flight manifest entry with the new hash and stat fields.
4. For every manifest entry NOT seen in the walk: the file was
   deleted. Drop it from the new tree.
5. For every file seen in the walk but NOT in the manifest: it's new.
   Hash, store, add to the new tree.
6. Build a new `Tree` object from the walk, write it.
7. Build a new `State` referencing the new tree, write it.
8. Update the thread head via `RefManager`.
9. Rewrite `manifest.toml` to reflect the captured state.

**Cost:** O(file count) `stat` calls + O(changed bytes) hash work. The
643-file workspace example takes about 5 ms to stat-walk and milliseconds
per changed file to rehash. A 5-line edit captures in ~10 ms total.

### Thread switch

`heddle thread switch <name>` (called via a shell hook, see below):

- If `<name>` is already materialized at its expected path: just
  `cd` there.
- Else: run the materialize flow for `<name>`, then `cd`.

Switches between already-materialized threads cost nothing
(`cd <path>` is free). First-time materializations cost the clonefile
walk (~91 ms for the heddle workspace).

### Thread drop

`heddle thread drop <name>`:

1. Walk the thread root, ensure no uncaptured changes (or accept
   `--force`).
2. `rm -rf <thread_root>`. APFS releases any blocks that aren't still
   shared with other threads or the canonical store.
3. Delete `<repo>/.heddle/threads/<name>/`.
4. Update the thread ref (or delete it; lifecycle handled by existing
   ref machinery).

### Shell integration

`heddle shell init zsh|bash|fish` emits a shell function that:

- Wraps `heddle start / heddle thread switch / heddle thread drop` to detect path changes and
  auto-`cd`.
- Sets `$HEDDLE_THREAD` to the current thread name (drives the
  prompt).
- Falls through to the real binary for every other subcommand.

User installs once (`heddle shell init zsh >> ~/.zshrc`). After that:

```
$ heddle start feature-x          # materialize + cd
[heddle:feature-x] $ cargo build         # vanilla speed
[heddle:feature-x] $ heddle thread switch main
[heddle:main]      $                     # cd'd back to main's root
```

Without the hook the user gets the dest path on stdout
(`heddle start feature-x --path <dir>`) and `cd`s themselves.

### CLI surface

Add a `--workspace` value:

| Value          | What it does                                                 | When to use                          |
|---             |---                                                           |---                                   |
| `materialized` | (new default) clonefile-or-copy from CAS into a thread dir   | Default for laptops with APFS/btrfs  |
| `virtualized`  | FUSE/FSKit/ProjFS mount over the CAS (today's "light")       | Very large repos, remote-backed CAS  |
| `solid`        | Full file copies, no shared extents (today's worktree)       | Strong isolation, ext4/NTFS hosts    |

`heddle start <name>` picks `materialized` by default when:
- The repo's filesystem supports reflinks (APFS, btrfs, XFS w/ reflinks,
  bcachefs, ReFS).
- The repo isn't flagged as virtualization-required (config knob for
  monorepos so big that materialize is infeasible).

Otherwise falls back to `virtualized`. Users can override explicitly.

## Data structures

### Thread manifest (per-thread, on disk)

`<repo>/.heddle/threads/<name>/manifest.toml`:

```toml
schema_version = 1
state_id = "hd-xxxxxxxx..."
tree_hash = "blake3..."
materialized_at = 1778800000

[files."src/main.rs"]
hash = "blake3..."
inode = 14735429
mtime_ns = 1778800123456789
ctime_ns = 1778800123456789
mode = 0o100644

[files."Cargo.toml"]
...
```

Lives outside the thread root so user actions (`rm -rf .` in the
thread) don't destroy the manifest. Rewritten atomically (`temp +
rename`) on every successful capture.

### Repository-side index

The `Repository` gains:

- `materialize_thread(&self, thread: &str, dest: &Path) -> Result<Manifest>`
- `capture_thread_from_disk(&self, thread: &str, root: &Path) -> Result<CaptureOutcome>`
- `thread_root_for(&self, thread: &str) -> PathBuf`

These compose `materialize_tree`, the manifest, and `RefManager`.
Most of the heavy lifting already exists in
`repository_materialization.rs`; the new code is glue.

### Daemon role

The daemon is **not strictly required** for this path — materialize and
capture are one-shot operations on locally-mmap'd state. But the daemon
can amortize:

- One open `Repository` handle (avoids `Repository::open` cost per
  command).
- Pack-file mmaps (so the next thread switch reuses them).
- The shared `BlobCachePool` (helps any in-process consumer; not
  directly the materialize path).
- Periodic GC of dropped threads.

The daemon does NOT live on the read path of any agent operation —
that's the whole point.

## Trade-offs and alternatives

### Why eager materialize over lazy virtualization

| Aspect            | Eager (this design)                          | Lazy (FSKit virtualized)                |
|---                |---                                           |---                                      |
| First read latency| ~91 ms (one-time setup)                      | 0 ms                                    |
| Steady-state read | Vanilla (no FS callbacks)                    | +5 ms/file FSKit IPC                    |
| Disk usage        | metadata + diverged blocks                   | zero                                    |
| Tool compat       | universal (real files)                       | depends on FSKit's coverage             |
| Implementation    | reuses `materialize_tree` (already shipped)  | full FSKit module + .appex chain        |
| Cross-platform    | mac/linux trivial, windows hard              | mac/linux/windows non-trivial each      |

For lightweight threads the eager path wins on every axis except "no
upfront cost." 91 ms upfront is below the perceptual threshold for
"instant" — the user types `heddle start X` and is in the new
thread before the shell prompt has fully redrawn.

### Why scan-on-capture over a write overlay

Three options were considered for capturing the agent's edits:

1. **Scan-on-capture** (this design): no FS hooks. `heddle capture`
   walks the worktree and uses a stat-cache to skip unchanged files.
2. **FSKit/FUSE overlay**: thin overlay that intercepts writes,
   auto-promotes to CAS on close. Reads pass through to the
   clonefiled APFS files.
3. **FSEvents/inotify watcher**: daemon watches the worktree.

Scan-on-capture wins because:
- It's the simplest. Zero kernel involvement on the write path.
- The stat-cache makes it fast: only changed files are re-hashed.
  Heddle's existing capture path already uses this pattern via
  `worktree_index`.
- It works with every tool that writes files — including those that
  use `mmap(MAP_SHARED)` to write, which the FSKit overlay path
  can't observe.
- It matches the mental model the user already has from git/jj
  ("changes don't exist until I run `commit`/`capture`").

The overlay path reintroduces FSKit specifically on the case we're
removing it from, which defeats the structural argument for this
design. The watcher path is unreliable on macOS under load (FSEvents
drops events; falling back to a scan defeats the point).

### Why a manifest sidecar

The alternative is keeping the captured state's tree object as the
ground truth and statting every file against the tree on capture.
That works but loses two things:

1. **Local stat-cache.** Tree entries don't store inodes/mtimes — they
   can't, because those are filesystem-specific. The manifest is the
   per-materialization shadow that records "what the OS told us at
   materialize time."
2. **Robustness to OOB modifications.** If a tool creates a file that
   wasn't in the captured tree, the manifest's "this is what I last
   wrote here" can detect it. Without the manifest we'd treat it the
   same as a captured-and-now-modified file (correct, but slower).

The manifest is small (~one line per file) and rewritten atomically.

## Failure modes and edge cases

- **Filesystem doesn't support reflinks.** `clonefile_or_copy` already
  falls back to `fs::copy`. We warn the user once during materialize
  that disk usage will be linear in tree size. They can re-run with
  `--workspace virtualized` for the FUSE/FSKit path.

- **Disk fills mid-materialize.** Write to
  `<thread_root>.tmp/`, atomic rename on success, cleanup on failure.
  The existing `Repository::materialize_tree` doesn't currently do
  this; we add it.

- **User deletes files in the thread.** Capture's tree-walk omits
  them; the manifest entry is dropped. Same semantics as git.

- **User edits a file that wasn't materialized
  yet** (e.g., touched a new path). Capture sees the new file via the
  walk, hashes it, adds to the new tree. No special handling.

- **User runs `cargo clean`.** Removes `target/`, which was never in
  the captured tree. Manifest unchanged. Good.

- **Two terminals on the same thread, both editing.** Same as git
  worktrees: both writes land on disk, capture is whoever runs
  `heddle capture` first. We don't try to be smarter than git here.

- **User runs `chmod +x foo`.** Mode change is captured because we
  record `mode` in the manifest and compare it on the scan. A bare
  mode-only change does NOT require re-hashing the content.

- **User does `rm -rf .` inside the thread.** Thread is broken; manifest
  is orphaned (lives in `.heddle/`, survived the wipe). Re-run
  `heddle start <thread>` — it's idempotent, re-materializes.

- **Capture races a still-open writer.** Window where the file is
  partially-written but the writer hasn't yet flushed mtime forward.
  Mitigations:
  - The walk's stat reads `mtime_ns + ctime_ns`; if a writer has the
    file open with pending writes the mtime usually updates as it
    writes. Not guaranteed, just typical.
  - `heddle capture --confidence 1.0` re-hashes every file regardless of
    stat-cache. For the user who explicitly wants belt-and-braces.
  - Long-term: the daemon can probe with `lsof` / per-platform open-fd
    queries and warn if a write is in flight.

- **APFS snapshot pins the old blocks.** If Time Machine has a
  snapshot that includes the original blocks, deleting a thread keeps
  the blocks reachable until the snapshot rolls off. Surfaceable via
  `heddle status --verbose` but not actionable from heddle. Document.

## Performance expectations

Original design (April 2026, on the existing 643-file heddle workspace):

| Op                                          | Time          |
|---                                          |---            |
| Materialize 643 files / 9.6 MiB tree        | 91 ms         |
| `cargo build` against materialized tree     | 48.68 s       |
| Same `cargo build` against vanilla checkout | 48.94 s       |
| Same `cargo build` through FSKit mount      | 50.11 s       |
| Thread switch (already-materialized → `cd`) | <1 ms (shell) |
| Thread switch (cold materialize)            | ~91 ms        |

Capture targets were derived from "643-file stat walk ≈ 5 ms" + an
estimate for hashing changed bytes; both were validated by the
implementation (see below).

### Measured against the synthetic bench (post-implementation)

Apple M3 Pro / APFS / release build /
[`crates/cli/benches/clonefile_threads.rs`](../../crates/cli/benches/clonefile_threads.rs)
and in-process probe matching the same shape (20 hashed
directories, ~64-byte files).

| File count | Cold materialize | Capture no-op (stat-cache) | Capture single edit |
|---:        |---:              |---:                        |---:                 |
|         1k |           186 ms |                       8 ms |               89 ms |
|        10k |          2.91 s  |                     103 ms |              284 ms |
|       100k |     (≈30 s est.) |                  (≈1 s est.) |          (~3 s est.) |

Numbers are post-3-codex-review-passes (commits through `911d64b`).
The earlier table reported `iter_batched` results that included the
recursive `TempDir` drop of the per-iteration materialised worktree
inside the timed region — at 10k files that destructor was 95% of
the reported time, drowning the actual routine. The bench has since
been switched to `iter_batched_ref` so the input drops outside the
measurement; the numbers above are the real routine cost. Probe-
binary results match within a few percent.

* **Cold materialize** = first time materializing the blobs in this
  store's lifetime; pays for one pack-read + decompress + loose-mirror
  write per unique blob, then clonefile to the worktree. Bounded by
  `~0.29 ms/file` post `AtomicWriteMode::NoSync` + read-side
  hash-verify; the pre-fix per-file cost was ~5 ms dominated by
  `sync_data` + `sync_directory` per loose mirror.
* **Capture no-op** = `heddle thread switch` from inside a
  freshly-materialized thread to another thread, or `heddle capture`
  in an unchanged worktree. The stat-cache fast no-op in
  `capture_thread_from_disk` short-circuits the entire hash cycle
  when every manifest entry's `(inode, mtime, ctime, mode)` still
  matches; cost is one `lstat` per tracked file plus a constant
  per-call manifest read (~55 ms for a 10k-entry manifest).
  Per-file rate is ~10 µs — the kernel's stat-syscall floor.
* **Capture single edit** = same scenario after touching one file.
  Stat-cache reuse via `build_tree_with_stat_cache` keeps the
  read+hash work bounded to the single changed file; constant
  overhead from the new state write + manifest refresh + ref
  update accounts for the per-file ratio being worse at 1k than
  at 10k (the constants amortize better over more files).

100k entries listed as estimates because `/tmp` on the bench host
runs out of inodes before the fixture finishes; run on a host with
≥4 GB of `/tmp` and the cold time scales linearly per file (the
warm path scales sub-linearly because parallel `clonefile` saturates
to ≈12 k clones/sec across cores on M-series APFS).

### Why these numbers, where they live in the codebase

* `materialize_tree` is the byte-mover: writes each blob as a
  loose-uncompressed cache mirror, then `clonefile`s into the
  worktree. The dominant cost on a fresh store is the cache-mirror
  write; on a warm store it's one `clonefile` syscall per file.
* `AtomicWriteMode::NoSync` + read-side hash verify in
  `FsStore::loose_blob_path` is what made cold materialize feasible
  at scale — `sync_data` alone on macOS APFS is `F_FULLFSYNC`-class
  (~5 ms per call), so promoting 10k blobs through a durable atomic
  write would take 50+ s of fsync wallclock.
* `populate_manifest_from_tree` runs *after* materialize and walks
  the just-written worktree with one `lstat` per file. At ≈10 µs
  per stat, 10k files is 100 ms — the bulk of the post-materialize
  manifest write.
* `stat_cache_no_op` is the fast no-op predicate the next capture
  hits: iterate the manifest, `lstat` each entry, bail on any
  mismatch. Pure stat walk — same cost as
  `populate_manifest_from_tree`.

Scaling:
- Materialize cold: linear in file count. ~0.29 ms/file × N.
- Capture no-op: linear in file count via `lstat`. ~10 µs/file × N.
  Constant per-call manifest TOML read (~55 ms at 10k entries)
  becomes the dominant term as N shrinks — at 1k files it's
  ~7× the stat-walk cost.
- Capture small edit: same ~10 µs/file × N stat walk plus the new
  state write + manifest refresh constants (~50 ms each).
  Changed-byte count drives the hash cost, not the total file
  count.

## Cross-platform story

### macOS (target #1)

`clonefile(2)` from `<sys/clonefile.h>`. APFS-only — HFS+, NTFS via
Boot Camp, and anything else: falls back to `fs::copy`. The existing
`clonefile_or_copy` handles this transparently.

### Linux (target #2)

`ioctl(FICLONE)` works on:
- **btrfs**: always
- **XFS**: when mounted with `reflink=1` (default since RHEL 8)
- **bcachefs**: always
- **ZFS-on-Linux**: with `reflink=on` (recent)
- **ext4**: not supported; falls back to copy

Same code path as macOS via `clonefile_or_copy`.

### Windows

ReFS has `FSCTL_DUPLICATE_EXTENTS_TO_FILE` — direct equivalent. NTFS
has no reflink primitive. For NTFS hosts the fallback is the existing
ProjFS path (virtualization stays here as the lightweight-thread
implementation).

### Implication for `--workspace materialized` default

| OS / FS                | Default behavior                       |
|---                     |---                                     |
| macOS APFS             | `materialized` (clonefile)             |
| Linux btrfs/XFS-reflink| `materialized` (FICLONE)               |
| Linux ext4             | `materialized` (slow, falls to copy) — warn the user once. Suggest `virtualized` for repos > N MiB |
| Windows ReFS           | `materialized` (DUPLICATE_EXTENTS)     |
| Windows NTFS           | `virtualized` (ProjFS) — no reflinks   |

## Migration path

`materialized` ships as the day-one default on reflink-capable hosts
(macOS APFS, Linux btrfs/XFS-reflink/bcachefs, Windows ReFS).
Virtualized stays as a flag for the very-large-repo case and remote-
backed CAS; we keep that path supported because both code paths are
production users from day one. The CLI auto-selects based on the
filesystem detection table above; users override with `--workspace`.

FSKit/FUSE/ProjFS are not deprecated. The structural argument is
that they're the right tool when blobs aren't all on local disk
(remote-fetch-on-demand) or when a tree is too large to fully
materialize. For the lightweight-thread workflow that is heddle's
headline, `materialized` is the right default and we ship it as
such.

## Open questions

1. **Should the manifest live inside or outside the thread root?**
   Outside (`.heddle/threads/<name>/manifest.toml`) survives `rm -rf .`
   in the worktree, which is the failure mode I expect to see most.
   Decision: outside.

2. **Should `heddle capture` re-validate file contents against the
   manifest periodically?** A user who runs
   `dd if=/dev/random of=foo bs=1 count=1 conv=notrunc` writes one
   byte; mtime updates; capture rehashes. Correct. But what if they
   manage to write without updating mtime (e.g., a buggy editor)?
   The manifest would skip the file. Mitigation: `--paranoid` flag,
   not a default. Decision: rely on stat-cache, ship the flag.

3. **GC of orphaned thread roots.** If a user `kill -9`s heddle
   mid-materialize, we leave `<thread_root>.tmp/` behind. Daemon
   sweep, or just always clean up `*.tmp` siblings on the next
   materialize. Decision: clean on next materialize + daemon sweep.

4. **What does `heddle thread switch` do when there are uncaptured
   changes in the current thread?** Auto-capture (jj-style). Every
   `thread switch` is a checkpoint: walk the current thread root with
   the stat-cache, write a new state if anything changed, advance
   the current thread's head, then switch. The user never has the
   "you have uncommitted changes" experience that git inflicts.
   Mirrors heddle's broader stance that the agent's edits are
   recoverable provenance and should never silently disappear.
   `--no-auto-capture` is the opt-out.

5. **Does the daemon need to know about materialized threads?** For
   its own bookkeeping (idle GC, status reporting), probably yes —
   add a parallel `materialized_threads.json` next to the existing
   `mounts.json`. Decision: yes, second iteration.

## What this does *not* solve

- **Repos so large the materialize is itself expensive.** Linux
  kernel: ~80k files. Chromium: ~400k. Materialize-walk at 1
  file/100µs is 8 s for the kernel, 40 s for Chromium. Bearable for
  Linux, unworkable for Chromium. For those: stay virtualized, or
  partial materialize (materialize only the subtrees the agent
  declares interest in).
- **Cross-repo block sharing.** APFS clonefile shares blocks within a
  filesystem only when the source-and-destination share an ancestor
  block. Two unrelated heddle repos that happen to have the same
  bytes on disk won't dedupe. Acceptable; cross-repo dedup is a
  separate problem.
- **Remote-backed CAS.** If the canonical loose blob isn't on local
  disk (because the repo is served by a remote CAS), there's nothing
  to clonefile. This is exactly the case virtualized mounts are
  good at. We don't try to do both.

## Implementation milestones

1. **Library function** (1 day): `Repository::materialize_thread` +
   `Repository::capture_thread_from_disk` + manifest read/write.
   Wire to existing `materialize_tree`. The prototype binary used in
   this design's benchmarks proved the math; the rest is shape.

2. **CLI surface** (1 day): `--workspace materialized` flag on
   `heddle start`, plumbed through `mount_lifecycle.rs`.
   Keep existing `light`/`solid` working.

3. **Shell hooks** (1 day): `heddle shell init {zsh,bash,fish}`,
   `$HEDDLE_THREAD` env var, prompt PS1 helpers.

4. **Capture integration** (2 days): `heddle capture` detects when
   it's run inside a materialized thread (sentinel + manifest
   present), runs the scan path instead of the worktree-walk path.

5. **Daemon awareness** (1 day): `materialized_threads.json` registry,
   `heddle daemon status` surfaces them.

6. **Tests + bench** (1 day): unit tests for manifest round-trip,
   integration tests for materialize→edit→capture loops, perf bench
   for materialize at 1k / 10k / 100k file counts.

Total: ~1 week of focused work to a default-flippable state. The
bottleneck is not code volume — it's the carefulness around the
capture semantics, which we should burn-in before flipping the
default.

## References

- `crates/repo/src/repository_materialization.rs` — the existing
  materializer (already clonefile-first).
- `crates/objects/src/fs_clone.rs` — `clonefile_or_copy` and the
  cross-platform reflink abstraction.
- `crates/objects/src/store/fs/fs_impl.rs` — `loose_blob_path`,
  `promote_to_loose_uncompressed`.
- `crates/cli/src/cli/commands/mount_lifecycle.rs` — existing
  workspace-selection plumbing; adds `materialized` as a sibling to
  `light`/`solid`.
- `docs/design/mount-daemon.md` — the daemon this work composes with.
- Apple, `clonefile(2)` man page.
- "Project Lightspeed" notes (internal): prior thinking on
  CAS-as-block-store. Aligns with the structural argument here.
