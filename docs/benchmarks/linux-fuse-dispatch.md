# Linux FUSE-backed thread dispatch — benchmark + recommendation

Tracking issue: [HeddleCo/heddle#164](https://github.com/HeddleCo/heddle/issues/164).
Bench harness: [`crates/devtools/src/fuse_dispatch_bench.rs`](../../crates/devtools/src/fuse_dispatch_bench.rs).

## TL;DR

**Do not migrate the HeddleCo orchestrator from `git worktree` dispatch
to `heddle start <name> --workspace virtualized` dispatch on Linux at this
time.** The Linux FUSE adapter today (`crates/mount/src/fuse.rs`)
implements read-path FUSE ops (`open`/`lookup`/`getattr`/`read`/
`readdir`) plus `write` to existing files, but does **not** implement
the namespace-mutating ops `create` / `mkdir` / `mknod` / `unlink` /
`rename` / `setattr`. The `fuser` crate defaults those to `ENOSYS`,
so any process running inside a virtualized mount that tries to
create a new file fails with `Function not implemented (os error 38)`
— including `cargo`, which must create `Cargo.lock` and every artifact
under `target/` before it can do anything useful.

`heddle start <name> --workspace solid` works on Linux today and is on par
with `git worktree` for the create/build matrix; once the FUSE write
side gains `create`/`mkdir`, the same harness will produce the
virtualized-mode numbers we need.

## Host

```
Ubuntu 24.04.4 LTS | kernel 6.8.0-71-generic | 8 threads | 15.6 GiB RAM | rootfs=ext2/ext3
cargo 1.95.0 (f2d3ce0bd 2026-03-21)
binary: heddle 0.2.4 (release, --features mount)
```

Notes:
- The rootfs `stat -f -c %T /` reports `ext2/ext3`, i.e. **no reflink
  support**, so `materialized` and `solid` workspace modes collapse to
  the same shape (full file copies). The bench therefore drives `solid`
  rather than `materialized` for the "heavy" column.
- The host kernel is 6.8.x, well past the 5.16 floor that the FUSE
  adapter's `FUSE_DIRECT_IO_ALLOW_MMAP` opt-in needs
  (`crates/mount/src/fuse.rs:288`).

## Reproduction (one command)

```bash
cargo run --release -p heddle-devtools -- fuse-dispatch-bench \
  --workload <path-to-cargo-workspace> \
  --heddle-bin "$(realpath target/release/heddle)" \
  --parallel 3 \
  --modes git,solid,virt \
  --stress-secs 30 \
  --json out.json --md out.md
```

The harness:
1. Snapshots `<workload>` into two source repos (one git, one
   `heddle`-initialized + captured) under `--out-dir` (default a
   tempdir).
2. For each requested mode: creates N parallel workdirs via
   `git worktree add` / `heddle start --workspace solid` /
   `heddle start --workspace virtualized`. For virtualized
   the `--path` argument is ignored by the CLI; the actual mount path
   is read from the JSON output (`thread.path` /
   `<parent>/.<repo-name>-heddle-mounts/<thread>`).
3. Runs `cargo check --workspace` (cold), touches one file,
   `cargo check --workspace` (incremental), then deletes the per-
   workdir `CARGO_TARGET_DIR` and runs `cargo build --release
   --workspace` (cold release). Each phase runs in parallel across
   the N workdirs.
4. Captures `du -sb` post-create and post-build per workdir.
5. (Optional) Runs a stress loop for `--stress-secs` seconds: each
   parallel worker rebuilds, edits one line, rebuilds, in a tight
   loop. Failures, first-iter time, and last-iter time per worker
   are captured along with FUSE-related `dmesg` lines.

The harness shells out to `git`, the user-provided `heddle` binary,
and `cargo`. It deliberately does not link against in-tree heddle
crates so it can benchmark a different binary than the one it was
built with.

## Results (synthetic small workload)

The numbers below come from `--workload /tmp/bench-workload`, a
single-package crate (one `lib.rs`, one `main.rs`, no deps). It is
**not** the full 80-crate heddle workspace the issue calls for —
running the full workspace under virtualized mode is currently
impossible (see "What virt fails on" below), so the small workload
was used to validate the harness end-to-end and capture the failure
shape. Once the FUSE write side lands, re-running with
`--workload <heddle>` will produce the workload-scaled numbers; the
harness change required is zero.

| mode  | create (agg) | cold check (agg) | incr check (agg) | cold release (agg) | disk post-build (per child) |
|-------|--------------|------------------|------------------|--------------------|-----------------------------|
| git   | 0.02 s       | 0.18 s           | 0.12 s           | 0.25 s             | 445 KiB                     |
| solid | 0.10 s       | 0.18 s           | 0.10 s           | 0.24 s             | 445 KiB                     |
| virt  | 0.11 s       | **FAIL (ENOSYS)** | —               | —                  | n/a                         |

(`agg` = wall-clock from first-child-start to last-child-finish across
3 parallel workers. Each child also returns its own per-child seconds;
see `out.json` / the per-child tables the harness emits.)

The git-vs-solid columns confirm the harness produces sensible numbers
(create is dominated by process-launch cost for both; cold/incremental
check and cold release are indistinguishable on a single-file workload
because the dominant cost is cargo's dependency resolution + rustc
startup, which is identical across modes). On ext4 with reflinks
absent, `solid` is full-copy, so the cost of the create step (~0.10 s
vs. git's ~0.02 s) is the cost of one `heddle start` invocation
including daemon round-trips, not a filesystem cost per se.

## What virt fails on

Every virtualized-mode cargo invocation in the matrix fails with the
same shape:

```
error: failed to write /tmp/.../bench-virt-0/Cargo.lock

Caused by:
  failed to open: /tmp/.../bench-virt-0/Cargo.lock

Caused by:
  Function not implemented (os error 38)
```

`os error 38` is `ENOSYS`. The mount is up (`mount | grep
heddle-mount` shows it, `getattr` works, `lookup` works, `read` works
— the cargo process is able to walk the tree and read `Cargo.toml`),
but the first time cargo tries to `open(O_CREAT)` a new file the FUSE
kernel driver returns `ENOSYS` because the userspace adapter has not
implemented `create`.

Concretely, the `impl Filesystem for FuseShell` in
`crates/mount/src/fuse.rs:271` defines only these methods:

```
init, open, lookup, getattr, read, readdir, write, flush, release, destroy
```

— no `create`, no `mkdir`, no `mknod`, no `unlink`, no `rename`,
no `setattr`. The `fuser` crate's default impls for these return
`ENOSYS` to the kernel, which surfaces in userspace exactly as the
`os error 38` cargo prints. `write` is implemented
(`crates/mount/src/fuse.rs:433`) so writes to **already-existing**
inodes work, but cargo's first action in a fresh workdir is to
create files, not modify them, so the write path never gets reached.

This is not a Linux-FUSE-kernel limitation; the kernel will happily
route `create` callbacks to userspace once the adapter declares the
op. The Mac numbers being good is consistent with this: the FSKit
backend (`crates/mount/src/fskit/mod.rs`) takes a different code
path, so the Linux-specific gap doesn't show up there.

## Stress test outcome

`--stress-secs 30 --modes git,solid,virt` chose virt as the stress
target (the harness prefers virt when available; it's the mode that
needs the stability data). All 3 parallel workers attempted iteration
0, all hit the same ENOSYS at `Cargo.lock`, none completed a single
build:

| metric          | per child  |
|-----------------|------------|
| iters completed | 0, 0, 0    |
| first iter (s)  | —          |
| last iter (s)   | —          |
| failures        | 18 (all ENOSYS on `Cargo.lock`) |

No FUSE-related lines appeared in `dmesg --ctime` post-run on this
host — the failure mode is purely userspace; the kernel-side FUSE
driver had nothing to complain about because the userspace adapter
returned `ENOSYS` cleanly.

The longer 1-hour stress called for in the issue would produce the
same shape: every iter 0 returns ENOSYS, regardless of duration. The
30-second cap above is sufficient to demonstrate the failure; there
is no degradation-over-time signal to chase until create/mkdir
actually land.

## Recommendation

**Hold the orchestrator on `git worktree` for now.** Concretely:

1. Keep the existing `make_worktree` / `write_cargo_redirect` flow in
   `HeddleCo/scripts/dispatch.py`. It works and the Mac vs. Linux
   delta on the underlying FUSE adapter is large enough that a
   migration now would just shift the orchestrator onto a broken
   substrate on the platform where most agents run.
2. If we want a halfway step before FUSE writes land: switch
   orchestrator dispatch to `heddle start <name> --workspace solid
   --shared-target` on Linux. The bench shows solid mode is within
   noise of git for create / check / build, and the orchestrator
   would get the heddle-side benefits (semantic merge, native undo,
   thread metadata) without depending on FUSE. On ext4 hosts this is
   just full-copy + shared cargo target, equivalent in disk cost to
   the current `git worktree + shared target` shape. (On a
   reflink-capable host like btrfs it would also gain CoW for the
   tree itself, but that's not what this VPS measures.)
3. To unblock virtualized dispatch on Linux, the FUSE adapter needs
   at minimum: `create`, `mkdir`, `mknod`, `unlink`, `setattr`,
   `rename`. `unlink` and `rename` are reachable through any cargo
   incremental rebuild (atomic rename of `.tmp` files into final
   names is the standard build-system pattern); without all six,
   virtualized mode will keep showing some flavor of ENOSYS or EIO
   for any non-toy workload. Once those land, re-run the same bench
   harness with `--workload <full-heddle>` and we'll have the
   apples-to-apples numbers needed to decide on the migration.

## Reproducing on a different host

The harness is intentionally side-effect-only and stateless between
runs (it cleans up stale FUSE mounts under `--out-dir` at startup).
To reproduce on a different Linux host:

1. Install `fusermount3` (Ubuntu: `apt install fuse3`). Confirm
   `/dev/fuse` exists and the user can mount FUSE filesystems.
2. Build a heddle binary with `--features mount`:
   `cargo build --release -p heddle-cli --bin heddle --features mount`.
3. Run the command in the "Reproduction" section above with the
   workload of your choice.

On a host with reflink-capable rootfs (btrfs / XFS+reflinks /
bcachefs), the harness will also exercise the materialized-vs-solid
distinction once you wire `--modes git,materialized,solid,virt` —
that requires a one-line change to add `Materialized` to the
`Mode` enum in `crates/devtools/src/fuse_dispatch_bench.rs`. Left
for follow-up rather than guessed-at; this host can't observe the
distinction.
