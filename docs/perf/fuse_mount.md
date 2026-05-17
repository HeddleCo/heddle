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
`crates/objects/`, `crates/repo/`, `crates/refs/`, the compare
script and its tests, the workflow itself, or any workspace-shared
build input (`Cargo.lock`, root `Cargo.toml`) — see the
`fuse-bench-gate` + `fuse-bench` jobs in
`.github/workflows/rust-tests.yml`. The gate also runs fail-closed
(i.e. runs the bench) if `git fetch` or `git diff` errors so a
flaky base-branch fetch can't silently remove perf coverage.

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

Numbers below come from the post-cold-cache-fix run (Codex r1 / PR
#91, [theme D](#fuse_e2e-cold-cache-calibration)). Every iter now
calls `mount.clear_blob_cache()`, so the bench measures the cold-
hydration path from the object store instead of the post-warmup
steady state. The shift is most visible on hydration-heavy paths
(`mmap_read`, `stat_storm`, `write_throughput`, `random_read`); the
already-cold workloads (`seq_read`, `concurrent_read`, `lifecycle`)
barely moved.

| Workload                      | heddle FUSE             | vanilla ext4           | ratio   | overhead  | budget   |
|-------------------------------|-------------------------|------------------------|---------|-----------|----------|
| `seq_read`                    | 96.46 ms (20.3 MiB/s)   | 3.80 ms (514 MiB/s)    | 25.4×   | +2440%    | ≤ 35×    |
| `mmap_read`                   | 214.7 ms (298 MiB/s)    | 13.42 ms (4.66 GiB/s)  | 16.0×   | +1500%    | ≤ 22×    |
| `concurrent_read/4`           | 18.77 ms (104 MiB/s)    | 1.53 ms (1.27 GiB/s)   | 12.2×   | +1124%    | ≤ 18×    |
| `concurrent_read/16`          | 17.57 ms (111 MiB/s)    | 1.53 ms (1.27 GiB/s)   | 11.4×   | +1045%    | ≤ 18×    |
| `random_read`                 | 61.99 ms ( 63 MiB/s)    | 1.49 ms (2.62 GiB/s)   | 41.6×   | +4063%    | ≤ 55×    |
| `stat_storm`                  | 328.9 ms (6.08 Kelem/s) | 6.24 ms (320 Kelem/s)  | 52.7×   | +5170%    | ≤ 70×    |
| `write_throughput`            | 24.64 ms (162 MiB/s)    | 1.55 ms (2.58 GiB/s)   | 15.9×   | +1490%    | ≤ 22×    |
| `lifecycle` (mount+read+drop) | 123.2 ms (8.1 elem/s)   | 0.310 ms (3227 elem/s) | 397×    | +39660%   | ≤ 700×   |

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

- **`seq_read` ≤ 35×** (actual 25.4×). Per-file open is a full FUSE
  round-trip; 500 opens dominate the per-iter wall-clock. Real
  cargo workloads hit this constantly. Anything worse than 35×
  suggests we regressed the open-fast-path or the blob cache
  stopped serving hot bytes.
- **`mmap_read` ≤ 22×** (actual 16.0×). With the cold-cache calibration
  every iter re-fetches the 64 MiB blob from the object store before
  the mapping warms, so the ratio is much higher than the previous
  warm-iter measurement (~4×). The `FUSE_DIRECT_IO_ALLOW_MMAP` cap
  shipped in
  [heddle#74](https://github.com/HeddleCo/heddle/pull/85) is what
  makes this number meaningful — without it,
  `mmap(MAP_SHARED, ...)` returns `ENODEV` and the workload doesn't
  run.
- **`concurrent_read/{4,16}` ≤ 18×** (actual 12.2× / 11.4×). Same
  per-open cost as `seq_read`, but `fuser`'s worker pool pipelines
  reads across threads — bringing per-iter wall-clock down ~5×
  vs the single-threaded `seq_read` for the same fixture. The
  16-thread case isn't meaningfully faster than 4 on a 4 vCPU
  runner; expect the gap to widen on a CI runner with more cores.
- **`random_read` ≤ 55×** (actual 41.6×). One open, then 1000
  random 4 KiB `pread`s on a freshly-cleared blob cache. Each
  read pays both the FUSE round-trip and the re-hydration miss,
  which is why the cold-cache calibration pushed the ratio from
  21× to ~42×.
- **`stat_storm` ≤ 70×** (actual 52.7×). Per-entry `metadata` is a
  FUSE `lookup` + `getattr` round-trip; 2000 entries × the FUSE
  cost dominates. The blob cache holds the captured tree, so
  clearing it forces a re-walk on every iter — the source of the
  ~3× jump under the cold-cache calibration.
- **`write_throughput` ≤ 22×** (actual 15.9×). Writes flow into the
  hot-tier buffer and promote on flush. The per-`write(2)` FUSE
  round-trip is the dominant cost; 4 MiB / 16 KiB chunks = 256
  round-trips, and the post-fix re-promote pass adds ~10 ms vs
  the previous warm number.
- **`lifecycle` ≤ 700×** (actual 397×). Vanilla
  `mkdir + write + read + drop` is ~310 µs; FUSE
  `mount + first-read + drop` is dominated by the kernel publish
  + `fusermount3` invocation, which is ~120 ms on this host. We
  tolerate a high ratio because the *absolute* number is what
  matters for daily use, not the ratio against an unrealistically
  cheap baseline. Cap by absolute time: ≤ 200 ms per
  mount-then-unmount cycle.

<a id="fuse_e2e-cold-cache-calibration"></a>

### Cold-cache calibration (Codex r1 / PR #91)

The original bench built the FUSE mount *once* outside the timed
loop, so the blob cache warmed after the first sample and later
samples measured warm-path numbers — a regression in cold
hydration would have been invisible to this gate. The fix
(`HeddleMount::cold()` → `ContentAddressedMount::clear_blob_cache()`
at the top of every iter) measures cold every sample. The budgets
above are post-fix; the pre-fix table is in the git history at
`f55693b` if you need to compare apples-to-apples against earlier
runs.

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
