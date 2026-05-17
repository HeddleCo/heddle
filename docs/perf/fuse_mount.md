# Linux FUSE mount — perf reference

This doc is the home of the `fuse_e2e` criterion-bench numbers and
the budgets we hold them to. Run the bench locally with:

```bash
cargo bench --features fuse -p heddle-mount --bench fuse_e2e
```

…and the committed regression gate with:

```bash
python3 scripts/fuse-bench-compare.py \
    --criterion-dir target/criterion \
    --baseline crates/mount/benches/fuse_e2e_baseline.json \
    --threshold 0.20
```

The same compare runs in CI on every PR touching `crates/mount/`,
`crates/objects/`, `crates/repo/`, or `crates/refs/` — see the
`fuse-bench` job in `.github/workflows/rust-tests.yml`.

## What the bench measures

`benches/fuse_e2e.rs` drives 7 workloads through a real
`FuseShell::mount_background` session (no stubs, no in-process
shortcuts), each paired with a vanilla-ext4 `std::fs` baseline:

| Group              | Models                                                  |
|--------------------|---------------------------------------------------------|
| `seq_read`         | `cargo check` walking 500 × 4 KiB `.rs` files.          |
| `mmap_read`        | rust-analyzer / ripgrep mmap-scanning a 64 MiB file.    |
| `concurrent_read`  | `cargo build -jN` / `rg -jN`: 4 and 16 threads.         |
| `random_read`      | DB-style random 4 KiB `pread` × 1000 against 16 MiB.    |
| `stat_storm`       | `find . -name X`: 2000 entries × `metadata`.            |
| `write_throughput` | Editor save: sequential 16 KiB-chunked 4 MiB write.     |
| `lifecycle`        | Mount-publish + first-read + drop-unmount, end-to-end.  |

The full design (size choices, scope decisions vs the issue, why
threads-not-processes for concurrency) is in the bench file's module
doc comment.

## Heddle FUSE vs vanilla ext4 — current numbers

Captured on the host listed in the `_meta` block of
`crates/mount/benches/fuse_e2e_baseline.json`. Criterion reports the
*mean* iteration time; throughput is derived from
`Throughput::Bytes` / `Throughput::Elements`.

<!--
PERF_TABLE_START — kept as a marker so the table can be regenerated
by future tooling (`scripts/fuse-bench-render-table.py`, planned).
Regen instructions:
  1. cargo bench --features fuse -p heddle-mount --bench fuse_e2e
  2. update crates/mount/benches/fuse_e2e_baseline.json with the
     new means (in ns_per_iter)
  3. by-hand: re-render the table below from the baseline + the
     known fixture sizes in benches/fuse_e2e.rs (SEQ_FILE_COUNT,
     MMAP_FILE_SIZE, etc.)
-->

| Workload                      | heddle FUSE             | vanilla ext4           | ratio   | overhead  | budget   |
|-------------------------------|-------------------------|------------------------|---------|-----------|----------|
| `seq_read`                    | 87.08 ms (22.4 MiB/s)   | 3.61 ms (541 MiB/s)    | 24.1×   | +2310%    | ≤ 35×    |
| `mmap_read`                   | 59.94 ms (1.04 GiB/s)   | 14.02 ms (4.46 GiB/s)  |  4.27×  | +327%     | ≤ 7×     |
| `concurrent_read/4`           | 18.55 ms (105 MiB/s)    | 1.59 ms (1.20 GiB/s)   | 11.7×   | +1070%    | ≤ 18×    |
| `concurrent_read/16`          | 16.82 ms (116 MiB/s)    | 1.54 ms (1.24 GiB/s)   | 10.9×   | +993%     | ≤ 18×    |
| `random_read`                 | 35.71 ms (109 MiB/s)    | 1.67 ms (2.28 GiB/s)   | 21.3×   | +2034%    | ≤ 32×    |
| `stat_storm`                  | 101.38 ms (19.7 Kelem/s)| 6.49 ms (308 Kelem/s)  | 15.6×   | +1462%    | ≤ 25×    |
| `write_throughput`            | 13.90 ms (288 MiB/s)    | 1.53 ms (2.55 GiB/s)   |  9.07×  | +807%     | ≤ 14×    |
| `lifecycle` (mount+read+drop) | 107.73 ms (9.3 elem/s)  | 0.230 ms (4350 elem/s) | 468×    | +46720%   | ≤ 700×   |

<!-- PERF_TABLE_END -->

## Budget rationale

The budgets in the rightmost column are how-bad-is-too-bad, not
how-good-we-want-to-be. They're picked from the first run of the
suite (the "ratio" column) plus ~50% headroom for the kind of
variance a shared CI runner produces. **Hitting a budget is *not*
what fails the `fuse-bench` CI job** — the actual gate is
`scripts/fuse-bench-compare.py`'s `+20% vs baseline` check against
`crates/mount/benches/fuse_e2e_baseline.json`. The budgets here are
the editorial "this is what we tolerate against vanilla" view for
roadmap / triage discussions; ratchet them down as the
implementation improves.

- **`seq_read` ≤ 35×** (actual 24.1×). Per-file open is a full FUSE
  round-trip; 500 opens dominate the per-iter wall-clock. Real
  cargo workloads hit this constantly. Anything worse than 35×
  suggests we regressed the open-fast-path or the blob cache
  stopped serving hot bytes.
- **`mmap_read` ≤ 7×** (actual 4.27×). The mapping path benefits
  from kernel page-cache reuse after the first fault, so heddle
  should be within a small constant factor of vanilla. The
  `FUSE_DIRECT_IO_ALLOW_MMAP` cap shipped in
  [heddle#74](https://github.com/HeddleCo/heddle/pull/85) is what
  makes this number meaningful — without it,
  `mmap(MAP_SHARED, ...)` returns `ENODEV` and the workload
  doesn't run.
- **`concurrent_read/{4,16}` ≤ 18×** (actual 11.7× / 10.9×). Same
  per-open cost as `seq_read`, but `fuser`'s worker pool pipelines
  reads across threads — bringing per-iter wall-clock down ~5×
  vs the single-threaded `seq_read` for the same fixture. The
  16-thread case isn't meaningfully faster than 4 on a 4 vCPU
  runner; expect the gap to widen on a CI runner with more cores.
- **`random_read` ≤ 32×** (actual 21.3×). One open, then 1000
  random 4 KiB `pread`s. Each read is a full FUSE round-trip even
  after the blob cache is warm (the cache hit is upstream of the
  FUSE syscall cost), so the per-read overhead is what dominates.
- **`stat_storm` ≤ 25×** (actual 15.6×). Per-entry `metadata` is a
  FUSE `lookup` + `getattr` round-trip. The attribute TTL (`TTL`
  const in `crates/mount/src/fuse.rs`) means the kernel caches
  between iters, but the first pass pays per-entry.
- **`write_throughput` ≤ 14×** (actual 9.07×). Writes flow into
  the hot-tier buffer and promote on flush. The per-`write(2)`
  FUSE round-trip is the dominant cost; 4 MiB / 16 KiB chunks =
  256 round-trips.
- **`lifecycle` ≤ 700×** (actual 468×). Vanilla
  `mkdir + write + read + drop` is ~230 µs; FUSE
  `mount + first-read + drop` is dominated by the kernel publish
  + `fusermount3` invocation, which is ~100 ms on this host. We
  tolerate a high ratio because the *absolute* number is what
  matters for daily use, not the ratio against an unrealistically
  cheap baseline. Cap by absolute time: ≤ 200 ms per
  mount-then-unmount cycle.

## Known limits

- **64 MiB cap on `mmap_read`.** `crates/repo/src/worktree_walk.rs::MAX_FILE_SIZE`
  is 100 MiB; anything larger fails `repo.snapshot` with
  `InvalidFileSize`. The issue's worked example of a 1 GiB file
  isn't currently a heddle-servable shape. If/when that cap moves,
  bump `MMAP_FILE_SIZE` in `benches/fuse_e2e.rs` to match.
- **Threads, not processes, for `concurrent_read`.** The kernel-
  side contention surface is the same (multiple in-flight `read`
  callbacks against one FUSE session), and threads keep the bench
  in-process so criterion can time it. The issue's "N processes"
  framing translates 1:1 to N threads for what we're measuring.
- **`drop_caches` not exercised.** Requires root; CI runners don't
  have it. Cold-cache behavior of the *blob cache* is covered by
  `mount_read_paths::bench_chunked_read_cold`; cold-cache behavior
  of the host page cache is a follow-up for when we run benches on
  a dedicated bare-metal runner.
- **No `heddle mount` / `heddle unmount` CLI in `lifecycle`.** The
  CLI is a thin wrapper around `FuseShell::mount_background` (see
  `crates/cli/src/cli/commands/mount_lifecycle.rs`); process-launch
  overhead would dominate and obscure the kernel-side cost we care
  about.

## Updating the baseline

Bump `crates/mount/benches/fuse_e2e_baseline.json` when an
intentional perf change lands — typically one of:

1. A kernel cap is added or removed (`FUSE_DIRECT_IO_ALLOW_MMAP`
   shape change, batched readdir, splice etc).
2. The blob cache size / promotion policy moves.
3. The FUSE worker pool sizing or attribute-TTL strategy changes.

Always ratchet *down* — never *up* — without a recorded reason. The
compare script will accept a faster number silently; a slower number
fails CI until you either fix the regression or amend the baseline
with justification.
