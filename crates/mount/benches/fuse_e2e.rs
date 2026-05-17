// SPDX-License-Identifier: Apache-2.0
//! Linux FUSE end-to-end benchmark (`fuse_e2e`).
//!
//! Unlike the sibling [`mount_read_paths`] bench (which exercises
//! `ContentAddressedMount` in-process, no kernel), this bench drives
//! the mount through a real `FuseShell::mount_background` session.
//! Every read flows: bench thread → kernel `read(2)` → FUSE driver →
//! `fuser` worker → `FuseShell::read` → `ContentAddressedMount::read`
//! → object store. The round-trip is what daily users pay; this bench
//! measures it.
//!
//! ## Workloads
//!
//! Mirrors [HeddleCo/heddle#89](https://github.com/HeddleCo/heddle/issues/89):
//!
//! | # | Group              | What it models                                                    |
//! |---|--------------------|-------------------------------------------------------------------|
//! | 1 | `seq_read`         | `cargo check`-shape sequential scan of every `.rs` file in a dir. |
//! | 2 | `mmap_read`        | rust-analyzer / ripgrep on a big file: `mmap` + linear scan.      |
//! | 3 | `concurrent_read`  | `cargo build -j N` / `rg`: N threads each scanning their own set. |
//! | 4 | `random_read`      | DB-style random 4 KiB pread into a single file (worst case).      |
//! | 5 | `stat_storm`       | `find . -name X`: readdir + per-entry stat.                       |
//! | 6 | `write_throughput` | `cargo build` writing rlibs: sequential 16 KiB-chunked write.     |
//! | 7 | `lifecycle`        | mount-publish + first-read + drop-unmount end-to-end latency.     |
//!
//! ## Baselines
//!
//! Each workload runs against:
//!
//!   * **heddle FUSE**  — the bench mounts a fresh repo per group.
//!   * **vanilla ext4** — the bench writes the same bytes into a
//!     plain `TempDir` and runs the equivalent `std::fs` calls.
//!
//! The delta is the FUSE+core overhead a daily user observes vs
//! running directly against `git`'s working tree. Git working tree
//! and ext4 are indistinguishable for read perf (both are loose
//! files in the page cache); we don't bench git separately for that
//! reason. NFS-loopback is a follow-up if/when we add a Linux NFS
//! fallback bench.
//!
//! ## Scope decisions (vs the issue's wording)
//!
//! * **Concurrent uses threads, not processes.** The kernel-side
//!   contention surface is the same: multiple in-flight `read`
//!   callbacks against a single FUSE session, which fuser
//!   multiplexes via its worker pool. Threads keep the bench
//!   in-process so criterion can time it; spawning N child
//!   processes adds IPC-for-timing complexity that doesn't change
//!   what's being measured.
//! * **Cold cache via `mount.clear_blob_cache()`, not `drop_caches`.**
//!   `echo 3 > /proc/sys/vm/drop_caches` needs root; CI runners
//!   don't have it. Clearing heddle's own blob cache is the
//!   heddle-relevant cold path — re-hydrating from the object
//!   store on the next read. The host page cache stays warm in
//!   both the heddle and vanilla columns, so the comparison is
//!   honest: warm-page-cache vanilla vs warm-page-cache + cold-
//!   blob-cache heddle.
//! * **Sizes are smaller than the issue's worked examples** (e.g.
//!   500 files vs 10k, 128 MiB vs 1 GiB, 2k stats vs 100k). Criterion
//!   runs each bench tens of times for statistical rigor, so we
//!   pick sizes that keep total wall-clock under ~5 min per group
//!   while still being large enough to exceed kernel-side
//!   readahead and FUSE-batch boundaries. The per-iteration shape
//!   is the same as the issue's worked examples; scale-up is
//!   linear and won't change the overhead ratio meaningfully.
//! * **Lifecycle is measured at the `FuseShell::mount_background` +
//!   drop boundary**, not via spawning `heddle mount` / `heddle
//!   unmount` subprocesses. The CLI is a thin wrapper around these
//!   exact calls (see `crates/cli/src/cli/commands/mount_lifecycle.rs`);
//!   process-launch overhead would dominate and obscure the
//!   kernel-side cost we care about.

#![cfg(all(target_os = "linux", feature = "fuse"))]

use std::{
    fs,
    hint::black_box,
    io::{Read, Write},
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use mount::{BackgroundSession, ContentAddressedMount};
use repo::Repository;
use tempfile::TempDir;

// ---------------------------------------------------------------------
// Fixture sizes
// ---------------------------------------------------------------------

/// Number of small source-shaped files in the sequential / concurrent
/// fixtures. 500 × 4 KiB ≈ 2 MiB, comfortably larger than L2 cache on
/// a 4 vCPU runner and big enough that per-file FUSE round-trips
/// dominate page-cache hits.
const SEQ_FILE_COUNT: usize = 500;
const SEQ_FILE_SIZE: usize = 4 * 1024;

/// Big-file mmap target: 64 MiB. Capped well below
/// `crates/repo/src/worktree_walk.rs::MAX_FILE_SIZE` (100 MiB) — the
/// repo refuses to snapshot anything larger, so the issue's worked
/// example of "1 GiB file" isn't currently a heddle-servable shape.
/// 64 MiB still spans thousands of 4 KiB pages so the FUSE `read`
/// path is exercised many times per iter, which is what the bench
/// measures. If/when the repo's per-blob cap moves, this constant
/// (and the docs/perf/fuse_mount.md "limits" section) should move
/// with it.
const MMAP_FILE_SIZE: usize = 64 * 1024 * 1024;

/// Random-read fixture: 16 MiB file, 1000 random 4 KiB reads per
/// iteration. The issue suggests 100 MiB / 10k reads — both linear
/// scale-ups that change wall-clock per iter but not the per-read
/// overhead being measured.
const RANDOM_FILE_SIZE: usize = 16 * 1024 * 1024;
const RANDOM_READ_COUNT: usize = 1000;
const RANDOM_READ_SIZE: usize = 4 * 1024;

/// Stat-storm fixture: 2000 entries (vs the issue's 100k, linear
/// scale-up). Each iter does readdir + per-entry `metadata`.
const STAT_ENTRY_COUNT: usize = 2000;

/// Write throughput fixture: 4 MiB sequential write, 16 KiB chunks.
/// Writes flow through the FUSE write callback into the hot tier,
/// then promote on flush — the daily path for editor saves.
const WRITE_SIZE: usize = 4 * 1024 * 1024;
const WRITE_CHUNK: usize = 16 * 1024;

// ---------------------------------------------------------------------
// Fixture builders
// ---------------------------------------------------------------------

/// Deterministic byte filler so heddle and vanilla columns compare
/// the exact same payloads.
fn fill(size: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(size);
    let mut x = 0x9E_37_79_B1_u32;
    while buf.len() < size {
        x = x.wrapping_mul(2_654_435_761);
        buf.extend_from_slice(&x.to_le_bytes());
    }
    buf.truncate(size);
    buf
}

/// Everything a bench group needs from a live heddle FUSE mount.
///
/// Drop order matters: `session` first (triggers unmount), then
/// `mountpoint` (removes the now-empty mountpoint dir), then
/// `repo_dir` (removes the source repo). Fields are declared in that
/// order because Rust drops fields top-to-bottom.
///
/// `repo_dir` is never read by bench bodies but must live as long as
/// the mount — the captured tree's loose-blob path resolves against
/// it. `#[allow(dead_code)]` reflects that intent; removing it would
/// segfault the FUSE worker once the snapshot disappears from disk.
#[allow(dead_code)]
struct HeddleMount {
    session: Option<BackgroundSession>,
    mountpoint: TempDir,
    repo_dir: TempDir,
}

impl HeddleMount {
    fn path(&self) -> &Path {
        self.mountpoint.path()
    }
}

impl Drop for HeddleMount {
    fn drop(&mut self) {
        // Explicit early-drop so any read errors from the bench body
        // surface before tempdir cleanup tries to descend into a
        // still-mounted directory.
        let _ = self.session.take();
    }
}

/// RED-COMMIT STUB: builds the repo + opens a `ContentAddressedMount`
/// but does **not** call `FuseShell::mount_background`. The
/// mountpoint stays an empty tempdir, so every bench body's read
/// fails with `ENOENT` and the criterion iteration panics — proving
/// the bench harness *would* catch a totally-broken mount. The green
/// commit swaps this body for a real `FuseShell::new(mount)
/// .mount_background(mountpoint.path())` call + kernel-publish wait.
fn build_heddle_mount(files: &[(String, Vec<u8>)]) -> HeddleMount {
    let repo_dir = TempDir::new().expect("tempdir for repo");
    let mountpoint = TempDir::new().expect("tempdir for mountpoint");
    let repo = Repository::init_default(repo_dir.path()).expect("init heddle repo");
    for (name, bytes) in files {
        fs::write(repo_dir.path().join(name), bytes).expect("write source file");
    }
    repo.snapshot(Some("fuse_e2e fixture".into()), None)
        .expect("snapshot fixture");
    let _ = ContentAddressedMount::new(repo, "main").expect("open mount");
    // INTENTIONAL STUB: no FUSE session. mountpoint is empty; every
    // read below will fail.
    HeddleMount {
        session: None,
        mountpoint,
        repo_dir,
    }
}

/// Build a vanilla ext4 baseline directory with the same files.
fn build_vanilla_dir(files: &[(String, Vec<u8>)]) -> TempDir {
    let dir = TempDir::new().expect("tempdir for vanilla");
    for (name, bytes) in files {
        fs::write(dir.path().join(name), bytes).expect("write vanilla file");
    }
    dir
}

/// Helper: read a single file to completion using `Read::read` in
/// `CHUNK`-sized buffers (default 16 KiB — kernel page-pair shape).
fn read_full(path: &Path, chunk: &mut [u8]) -> u64 {
    let mut f = fs::File::open(path).expect("open file");
    let mut total: u64 = 0;
    loop {
        let n = f.read(chunk).expect("read");
        if n == 0 {
            break;
        }
        total += n as u64;
    }
    total
}

// Cargo-shape file list: src_000.rs ... src_NNN.rs.
fn cargo_shape_files(count: usize, size: usize) -> Vec<(String, Vec<u8>)> {
    let payload = fill(size);
    (0..count)
        .map(|i| (format!("src_{i:04}.rs"), payload.clone()))
        .collect()
}

// Big-single-file list.
fn single_big_file(name: &str, size: usize) -> Vec<(String, Vec<u8>)> {
    vec![(name.to_string(), fill(size))]
}

// ---------------------------------------------------------------------
// 1. Sequential read (cargo-shape)
// ---------------------------------------------------------------------

fn bench_seq_read(c: &mut Criterion) {
    let files = cargo_shape_files(SEQ_FILE_COUNT, SEQ_FILE_SIZE);
    let total_bytes = (SEQ_FILE_COUNT * SEQ_FILE_SIZE) as u64;

    let heddle = build_heddle_mount(&files);
    let vanilla = build_vanilla_dir(&files);

    let mut group = c.benchmark_group("seq_read");
    group.throughput(Throughput::Bytes(total_bytes));
    // Each iter walks 500 files — readdir + open + read every one of
    // them through the kernel. Keep the sample count low so total
    // wall-clock per group stays bounded.
    group.sample_size(10);

    let heddle_root = heddle.path().to_path_buf();
    let mut chunk = vec![0u8; 16 * 1024];
    group.bench_function("heddle", |b| {
        b.iter(|| {
            let mut total: u64 = 0;
            for entry in fs::read_dir(&heddle_root).expect("readdir heddle") {
                let entry = entry.expect("dir entry");
                total += read_full(&entry.path(), &mut chunk);
            }
            assert_eq!(total, total_bytes);
            black_box(total);
        });
    });

    let vanilla_root = vanilla.path().to_path_buf();
    group.bench_function("vanilla", |b| {
        b.iter(|| {
            let mut total: u64 = 0;
            for entry in fs::read_dir(&vanilla_root).expect("readdir vanilla") {
                let entry = entry.expect("dir entry");
                total += read_full(&entry.path(), &mut chunk);
            }
            assert_eq!(total, total_bytes);
            black_box(total);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------
// 2. Mmap-based read
// ---------------------------------------------------------------------

fn bench_mmap_read(c: &mut Criterion) {
    let files = single_big_file("big.bin", MMAP_FILE_SIZE);

    let heddle = build_heddle_mount(&files);
    let vanilla = build_vanilla_dir(&files);

    let mut group = c.benchmark_group("mmap_read");
    group.throughput(Throughput::Bytes(MMAP_FILE_SIZE as u64));
    group.sample_size(10);

    let heddle_file = heddle.path().join("big.bin");
    group.bench_function("heddle", |b| {
        b.iter(|| {
            let f = fs::File::open(&heddle_file).expect("open heddle");
            // SAFETY: we hold the only handle for the lifetime of the
            // mapping; FUSE shell doesn't expose setattr(size), so
            // truncation through the mount is impossible.
            let map = unsafe { memmap2::Mmap::map(&f) }.expect("mmap heddle");
            // Sum byte-by-byte through black_box so LLVM can't lift
            // the read out. Volatile-ish without sprinkling intrinsics.
            let mut acc: u64 = 0;
            for chunk in map.chunks(4096) {
                acc = acc.wrapping_add(black_box(chunk[0]) as u64);
            }
            black_box(acc);
        });
    });

    let vanilla_file = vanilla.path().join("big.bin");
    group.bench_function("vanilla", |b| {
        b.iter(|| {
            let f = fs::File::open(&vanilla_file).expect("open vanilla");
            let map = unsafe { memmap2::Mmap::map(&f) }.expect("mmap vanilla");
            let mut acc: u64 = 0;
            for chunk in map.chunks(4096) {
                acc = acc.wrapping_add(black_box(chunk[0]) as u64);
            }
            black_box(acc);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------
// 3. Concurrent sequential reads
// ---------------------------------------------------------------------

fn bench_concurrent_read(c: &mut Criterion) {
    let files = cargo_shape_files(SEQ_FILE_COUNT, SEQ_FILE_SIZE);
    let total_bytes = (SEQ_FILE_COUNT * SEQ_FILE_SIZE) as u64;

    let heddle = build_heddle_mount(&files);
    let vanilla = build_vanilla_dir(&files);

    let mut group = c.benchmark_group("concurrent_read");
    group.throughput(Throughput::Bytes(total_bytes));
    group.sample_size(10);

    // Two thread counts. 64 (the issue's upper bound) is too noisy on
    // a 4-vCPU CI runner — context switching dominates and obscures
    // the FUSE overhead we're measuring.
    for &threads in &[4usize, 16] {
        let heddle_root = Arc::new(heddle.path().to_path_buf());
        group.bench_with_input(
            BenchmarkId::new("heddle", threads),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    run_concurrent(&heddle_root, threads, SEQ_FILE_COUNT);
                });
            },
        );

        let vanilla_root = Arc::new(vanilla.path().to_path_buf());
        group.bench_with_input(
            BenchmarkId::new("vanilla", threads),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    run_concurrent(&vanilla_root, threads, SEQ_FILE_COUNT);
                });
            },
        );
    }

    group.finish();
}

/// Spawn `threads` workers; each scans its own slice of `src_*.rs`
/// files (round-robin), reading every file to completion. Joins all
/// before returning. Modelling `cargo build -j N` / `rg -j N`.
fn run_concurrent(root: &Arc<PathBuf>, threads: usize, file_count: usize) {
    let handles: Vec<_> = (0..threads)
        .map(|tid| {
            let root = Arc::clone(root);
            thread::spawn(move || {
                let mut chunk = vec![0u8; 16 * 1024];
                let mut total: u64 = 0;
                let mut i = tid;
                while i < file_count {
                    let path = root.join(format!("src_{i:04}.rs"));
                    total += read_full(&path, &mut chunk);
                    i += threads;
                }
                total
            })
        })
        .collect();
    let mut grand: u64 = 0;
    for h in handles {
        grand += h.join().expect("worker thread panicked");
    }
    black_box(grand);
}

// ---------------------------------------------------------------------
// 4. Random read (4 KiB blocks)
// ---------------------------------------------------------------------

fn bench_random_read(c: &mut Criterion) {
    let files = single_big_file("rand.bin", RANDOM_FILE_SIZE);

    let heddle = build_heddle_mount(&files);
    let vanilla = build_vanilla_dir(&files);

    let mut group = c.benchmark_group("random_read");
    group.throughput(Throughput::Bytes((RANDOM_READ_COUNT * RANDOM_READ_SIZE) as u64));
    group.sample_size(10);

    // Pre-compute the offset schedule outside the timed loop. Weyl-
    // sequence stride gives a deterministic pseudo-random walk that
    // doesn't repeat for ITERS « FILE_SIZE / PAGE.
    let offsets: Vec<u64> = (0..RANDOM_READ_COUNT)
        .map(|i| {
            let max_off = RANDOM_FILE_SIZE.saturating_sub(RANDOM_READ_SIZE);
            ((i.wrapping_mul(0x9E37_79B1)) % max_off) as u64
        })
        .collect();

    let heddle_file = heddle.path().join("rand.bin");
    group.bench_function("heddle", |b| {
        let f = fs::File::open(&heddle_file).expect("open heddle");
        let mut buf = vec![0u8; RANDOM_READ_SIZE];
        b.iter(|| {
            let mut acc: u64 = 0;
            for &off in &offsets {
                let n = f.read_at(&mut buf, off).expect("pread heddle");
                acc = acc.wrapping_add(n as u64);
            }
            black_box(acc);
        });
    });

    let vanilla_file = vanilla.path().join("rand.bin");
    group.bench_function("vanilla", |b| {
        let f = fs::File::open(&vanilla_file).expect("open vanilla");
        let mut buf = vec![0u8; RANDOM_READ_SIZE];
        b.iter(|| {
            let mut acc: u64 = 0;
            for &off in &offsets {
                let n = f.read_at(&mut buf, off).expect("pread vanilla");
                acc = acc.wrapping_add(n as u64);
            }
            black_box(acc);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------
// 5. Stat-storm (readdir + per-entry stat)
// ---------------------------------------------------------------------

fn bench_stat_storm(c: &mut Criterion) {
    // Tiny files — the stat path doesn't care about content size, it
    // cares about how many entries fit in one readdir batch.
    let files: Vec<(String, Vec<u8>)> = (0..STAT_ENTRY_COUNT)
        .map(|i| (format!("e_{i:05}.dat"), vec![0u8; 16]))
        .collect();

    let heddle = build_heddle_mount(&files);
    let vanilla = build_vanilla_dir(&files);

    let mut group = c.benchmark_group("stat_storm");
    group.throughput(Throughput::Elements(STAT_ENTRY_COUNT as u64));
    group.sample_size(10);

    let heddle_root = heddle.path().to_path_buf();
    group.bench_function("heddle", |b| {
        b.iter(|| {
            let mut count = 0usize;
            for entry in fs::read_dir(&heddle_root).expect("readdir heddle") {
                let entry = entry.expect("dir entry");
                let m = fs::metadata(entry.path()).expect("metadata heddle");
                count += 1;
                black_box(m.len());
            }
            assert_eq!(count, STAT_ENTRY_COUNT);
        });
    });

    let vanilla_root = vanilla.path().to_path_buf();
    group.bench_function("vanilla", |b| {
        b.iter(|| {
            let mut count = 0usize;
            for entry in fs::read_dir(&vanilla_root).expect("readdir vanilla") {
                let entry = entry.expect("dir entry");
                let m = fs::metadata(entry.path()).expect("metadata vanilla");
                count += 1;
                black_box(m.len());
            }
            assert_eq!(count, STAT_ENTRY_COUNT);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------
// 6. Write throughput
// ---------------------------------------------------------------------

fn bench_write_throughput(c: &mut Criterion) {
    // Fixture starts with an empty target file (size matches WRITE_SIZE
    // so the write path is overwrite-in-place rather than grow-the-
    // file; the shell's hot-tier buffers either way).
    let files = vec![("out.bin".to_string(), vec![0u8; WRITE_SIZE])];

    let heddle = build_heddle_mount(&files);
    let vanilla = build_vanilla_dir(&files);

    let payload = fill(WRITE_SIZE);

    let mut group = c.benchmark_group("write_throughput");
    group.throughput(Throughput::Bytes(WRITE_SIZE as u64));
    group.sample_size(10);

    let heddle_file = heddle.path().join("out.bin");
    group.bench_function("heddle", |b| {
        b.iter(|| {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .open(&heddle_file)
                .expect("open heddle for write");
            let mut written: usize = 0;
            while written < WRITE_SIZE {
                let end = (written + WRITE_CHUNK).min(WRITE_SIZE);
                let n = f.write(&payload[written..end]).expect("write heddle");
                written += n;
            }
            // Drop closes → flush → release through the FUSE shell.
            drop(f);
            black_box(written);
        });
    });

    let vanilla_file = vanilla.path().join("out.bin");
    group.bench_function("vanilla", |b| {
        b.iter(|| {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .open(&vanilla_file)
                .expect("open vanilla for write");
            let mut written: usize = 0;
            while written < WRITE_SIZE {
                let end = (written + WRITE_CHUNK).min(WRITE_SIZE);
                let n = f.write(&payload[written..end]).expect("write vanilla");
                written += n;
            }
            drop(f);
            black_box(written);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------
// 7. Lifecycle latency
// ---------------------------------------------------------------------

fn bench_lifecycle(c: &mut Criterion) {
    // Small fixture so the bench measures mount+unmount, not blob
    // hydration. Single file, the same shape as `mount_fixture` in
    // tests/fuse_mount.rs.
    let files: Vec<(String, Vec<u8>)> = vec![("hello.txt".into(), b"world".to_vec())];

    let mut group = c.benchmark_group("lifecycle");
    group.throughput(Throughput::Elements(1));
    // Each iter builds + tears down a full mount. Keep sample count
    // low so we don't burn minutes here.
    group.sample_size(15);
    group.measurement_time(Duration::from_secs(20));

    group.bench_function("heddle_mount_unmount", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let start = Instant::now();
                let m = build_heddle_mount(&files);
                // First successful read = proof the kernel has
                // published the FS. Lifecycle latency must include
                // this; without it, "mount returned" doesn't mean
                // anything.
                let target = m.path().join("hello.txt");
                let _ = wait_for_first_read(&target, Duration::from_secs(5))
                    .expect("first read must succeed within 5s");
                drop(m); // triggers unmount
                total += start.elapsed();
            }
            total
        });
    });

    // Vanilla baseline: equivalent "create dir, write file, read file,
    // remove dir" sequence on plain ext4. Establishes the lower
    // bound for mountpoint setup + teardown.
    group.bench_function("vanilla_mkdir_unlink", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let start = Instant::now();
                let dir = TempDir::new().expect("tempdir vanilla");
                let path = dir.path().join("hello.txt");
                fs::write(&path, b"world").expect("write vanilla");
                let bytes = fs::read(&path).expect("read vanilla");
                assert_eq!(bytes, b"world");
                drop(dir);
                total += start.elapsed();
            }
            total
        });
    });

    group.finish();
}

/// Poll until `target` is readable, or return `Err` after `dur`.
/// Lifecycle bench uses this to charge the kernel publish window to
/// the mount-latency total.
fn wait_for_first_read(target: &Path, dur: Duration) -> std::io::Result<Vec<u8>> {
    let deadline = Instant::now() + dur;
    loop {
        match fs::read(target) {
            Ok(bytes) => return Ok(bytes),
            Err(_) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(e) => return Err(e),
        }
    }
}

criterion_group!(
    benches,
    bench_seq_read,
    bench_mmap_read,
    bench_concurrent_read,
    bench_random_read,
    bench_stat_storm,
    bench_write_throughput,
    bench_lifecycle,
);
criterion_main!(benches);
