// SPDX-License-Identifier: Apache-2.0
//! `PlatformShell` microbench: heddle mount vs vanilla `std::fs`.
//!
//! These benches measure the Rust core in isolation — no kernel, no
//! FSKit, no FUSE. The aim is to make `ContentAddressedMount`'s read
//! paths as close to `std::fs` time as possible so the per-platform
//! adapters (which all go through this same core) have the smallest
//! possible delta to make up.
//!
//! The bench fixture builds a heddle repo with files at four sizes
//! that mimic the working set a typical `cargo check` touches:
//!
//! | Tier | Size  | Models                |
//! |------|-------|-----------------------|
//! | tiny | 1 KB  | `Cargo.toml`, `*.toml`|
//! | src  | 64 KB | medium `.rs` file     |
//! | meta | 1 MB  | `.rmeta` for a dep    |
//! | rlib | 10 MB | linked `.rlib`        |
//!
//! For each tier we exercise:
//!  * **full_read** — one `read` call covering the whole file.
//!  * **chunked_read** — kernel-page-sized reads (16 KB) start-to-end.
//!  * **mmap_pattern** — 64 random-offset 4 KB reads against the same
//!    file, modelling `mmap` page faults on a hot working set.
//!  * **enumerate_then_attrs** — `enumerate(root)` followed by
//!    `attrs` for every entry, modelling `ls -la`.
//!
//! The vanilla-FS baseline writes the same bytes to a tempdir and
//! runs the equivalent `std::fs` calls. Difference between the two
//! groups is the cost we're trying to drive down.

use std::{
    fs,
    hint::black_box,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use heddle_format::compression::{CompressionConfig, compress, decompress};
use mount::{
    BlobCachePool, ContentAddressedMount, MountOptions, NodeId, PlatformShell, PromotionPolicy,
};
use repo::Repository;
use tempfile::TempDir;

/// Disable the idle-promotion sweep worker. Its 50 ms shutdown-poll
/// adds tens of ms to every `Drop` and was masking the prewarm
/// signal in earlier runs of the bench.
fn no_sweep() -> PromotionPolicy {
    PromotionPolicy {
        idle_after: Duration::from_secs(60),
        sweep_interval: None,
    }
}

// ---------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Tier {
    name: &'static str,
    size: usize,
    /// File name inside the fixture repo.
    path: &'static str,
}

const TIERS: &[Tier] = &[
    Tier {
        name: "tiny_1k",
        size: 1024,
        path: "Cargo.toml",
    },
    Tier {
        name: "src_64k",
        size: 64 * 1024,
        path: "src.rs",
    },
    Tier {
        name: "meta_1m",
        size: 1024 * 1024,
        path: "dep.rmeta",
    },
    Tier {
        name: "rlib_10m",
        size: 10 * 1024 * 1024,
        path: "dep.rlib",
    },
];

/// Deterministic byte sequence — same content for both the heddle
/// fixture and the vanilla baseline so we're comparing apples to
/// apples (byte-for-byte identical inputs).
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

/// Build a heddle repo populated with the tier files, snapshot it,
/// and return a mount opened on the captured thread.
fn build_mount_fixture() -> (TempDir, ContentAddressedMount) {
    let temp = TempDir::new().expect("tempdir");
    let repo = Repository::init_default(temp.path()).expect("init heddle repo");
    for tier in TIERS {
        fs::write(temp.path().join(tier.path), fill(tier.size)).expect("write fixture file");
    }
    repo.snapshot(Some("bench fixture".into()), None)
        .expect("snapshot");
    let mount = ContentAddressedMount::new(repo, "main").expect("open mount");
    (temp, mount)
}

/// Build a parallel vanilla-FS fixture. Same bytes, same names,
/// no heddle layer.
fn build_vanilla_fixture() -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    for tier in TIERS {
        fs::write(temp.path().join(tier.path), fill(tier.size)).expect("write vanilla file");
    }
    temp
}

/// Resolve a fixture file's NodeId via the public `lookup_path`
/// helper. Hoisted so the benchmark loop doesn't include the path
/// walk — we want to measure `read`, not `lookup`.
fn resolve_node(mount: &ContentAddressedMount, path: &str) -> NodeId {
    mount.lookup_path(path).expect("resolve fixture node")
}

// ---------------------------------------------------------------------
// Bench bodies
// ---------------------------------------------------------------------

fn bench_full_read(c: &mut Criterion) {
    let (_temp_repo, mount) = build_mount_fixture();
    let vanilla = build_vanilla_fixture();

    let mut group = c.benchmark_group("full_read");
    for tier in TIERS {
        group.throughput(Throughput::Bytes(tier.size as u64));

        // heddle: shell.read once with a buffer large enough for the
        // whole file. Mirrors how FUSE/FSKit deliver a single read
        // for a small file.
        let node = resolve_node(&mount, tier.path);
        let mut buf = vec![0u8; tier.size];
        group.bench_with_input(BenchmarkId::new("heddle", tier.name), &node, |b, node| {
            b.iter(|| {
                let n = mount
                    .read(*node, 0, black_box(&mut buf))
                    .expect("heddle read");
                debug_assert_eq!(n, tier.size);
                black_box(n);
            });
        });

        // vanilla: std::fs::read into a reused buffer.
        let path = vanilla.path().join(tier.path);
        let mut buf2 = vec![0u8; tier.size];
        group.bench_with_input(BenchmarkId::new("vanilla", tier.name), &path, |b, path| {
            b.iter(|| {
                let mut f = fs::File::open(path).expect("vanilla open");
                let n = f.read(black_box(&mut buf2)).expect("vanilla read");
                debug_assert_eq!(n, tier.size);
                black_box(n);
            });
        });
    }
    group.finish();
}

fn bench_chunked_read(c: &mut Criterion) {
    let (_temp_repo, mount) = build_mount_fixture();
    let vanilla = build_vanilla_fixture();

    // Apple Silicon page size — matches the kernel-side read chunks
    // FSKit will deliver under a sequential `cat`.
    const CHUNK: usize = 16 * 1024;

    let mut group = c.benchmark_group("chunked_read");
    for tier in TIERS {
        group.throughput(Throughput::Bytes(tier.size as u64));

        let node = resolve_node(&mount, tier.path);
        let mut chunk = vec![0u8; CHUNK];
        group.bench_with_input(BenchmarkId::new("heddle", tier.name), &node, |b, node| {
            b.iter(|| {
                let mut offset = 0u64;
                while offset < tier.size as u64 {
                    let n = mount
                        .read(*node, offset, black_box(&mut chunk))
                        .expect("heddle read");
                    if n == 0 {
                        break;
                    }
                    offset += n as u64;
                }
                black_box(offset);
            });
        });

        let path = vanilla.path().join(tier.path);
        let mut chunk2 = vec![0u8; CHUNK];
        group.bench_with_input(BenchmarkId::new("vanilla", tier.name), &path, |b, path| {
            b.iter(|| {
                let mut f = fs::File::open(path).expect("vanilla open");
                let mut total = 0usize;
                loop {
                    let n = f.read(black_box(&mut chunk2)).expect("vanilla read");
                    if n == 0 {
                        break;
                    }
                    total += n;
                }
                black_box(total);
            });
        });
    }
    group.finish();
}

fn bench_mmap_pattern(c: &mut Criterion) {
    let (_temp_repo, mount) = build_mount_fixture();
    let vanilla = build_vanilla_fixture();

    // Page size + iteration count tuned to mimic a linker walking a
    // .rlib via mmap: ~64 4 KB page faults per file in pseudo-random
    // order. Anything bigger than the file collapses to the head.
    const PAGE: usize = 4 * 1024;
    const ITERS: usize = 64;

    let mut group = c.benchmark_group("mmap_pattern");
    for tier in TIERS {
        // Total bytes touched per iteration. Lets `cargo criterion`
        // display per-byte rates that line up with `chunked_read`.
        group.throughput(Throughput::Bytes((PAGE * ITERS) as u64));

        let offsets: Vec<u64> = (0..ITERS)
            .map(|i| {
                let max_offset = tier.size.saturating_sub(PAGE);
                if max_offset == 0 {
                    0
                } else {
                    // Cheap deterministic shuffle — Weyl-sequence
                    // stride coprime with typical sizes.
                    ((i.wrapping_mul(0x9E3779B1)) % max_offset) as u64
                }
            })
            .collect();

        let node = resolve_node(&mount, tier.path);
        let mut page = vec![0u8; PAGE];
        group.bench_with_input(BenchmarkId::new("heddle", tier.name), &node, |b, node| {
            b.iter(|| {
                for off in &offsets {
                    let n = mount
                        .read(*node, *off, black_box(&mut page))
                        .expect("heddle read");
                    black_box(n);
                }
            });
        });

        let path = vanilla.path().join(tier.path);
        let mut page2 = vec![0u8; PAGE];
        group.bench_with_input(BenchmarkId::new("vanilla", tier.name), &path, |b, path| {
            b.iter(|| {
                let mut f = fs::File::open(path).expect("vanilla open");
                for off in &offsets {
                    f.seek(SeekFrom::Start(*off)).expect("vanilla seek");
                    let n = f.read(black_box(&mut page2)).expect("vanilla read");
                    black_box(n);
                }
            });
        });
    }
    group.finish();
}

fn bench_enumerate_then_attrs(c: &mut Criterion) {
    let (_temp_repo, mount) = build_mount_fixture();
    let vanilla = build_vanilla_fixture();

    let mut group = c.benchmark_group("enumerate_then_attrs");

    group.bench_function("heddle", |b| {
        b.iter(|| {
            let entries = mount.enumerate(NodeId::ROOT).expect("heddle enumerate");
            for entry in &entries {
                let _ = black_box(mount.attrs(entry.node).expect("heddle attrs"));
            }
            black_box(entries.len());
        });
    });

    let dir = vanilla.path().to_path_buf();
    group.bench_function("vanilla", |b| {
        b.iter(|| {
            let mut paths: Vec<PathBuf> = Vec::new();
            for entry in fs::read_dir(&dir).expect("vanilla readdir") {
                let entry = entry.expect("vanilla entry");
                paths.push(entry.path());
            }
            for path in &paths {
                let _ = black_box(fs::metadata(path).expect("vanilla metadata"));
            }
            black_box(paths.len());
        });
    });

    group.finish();
}

/// Cold-cache variant of `chunked_read`: each iteration clears the
/// blob cache so the first chunk pays the object-store hydration
/// cost. This is the honest number for the moment cargo first opens
/// a file it hasn't touched yet; the warm `chunked_read` bench
/// measures every read after that. The host OS page cache is left
/// hot — vanilla is already in the page cache so the two columns
/// compare "first heddle touch" vs "warm-page-cache vanilla".
fn bench_chunked_read_cold(c: &mut Criterion) {
    let (_temp_repo, mount) = build_mount_fixture();
    let vanilla = build_vanilla_fixture();

    const CHUNK: usize = 16 * 1024;

    let mut group = c.benchmark_group("chunked_read_cold");

    for tier in TIERS {
        group.throughput(Throughput::Bytes(tier.size as u64));

        let node = resolve_node(&mount, tier.path);
        let mut chunk = vec![0u8; CHUNK];
        group.bench_function(BenchmarkId::new("heddle", tier.name), |b| {
            b.iter(|| {
                mount.clear_blob_cache();
                let mut offset = 0u64;
                while offset < tier.size as u64 {
                    let n = mount
                        .read(node, offset, black_box(&mut chunk))
                        .expect("heddle read");
                    if n == 0 {
                        break;
                    }
                    offset += n as u64;
                }
                black_box(offset);
            });
        });

        let path = vanilla.path().join(tier.path);
        let mut chunk2 = vec![0u8; CHUNK];
        group.bench_function(BenchmarkId::new("vanilla", tier.name), |b| {
            b.iter(|| {
                let mut f = fs::File::open(&path).expect("vanilla open");
                let mut total = 0usize;
                loop {
                    let n = f.read(black_box(&mut chunk2)).expect("vanilla read");
                    if n == 0 {
                        break;
                    }
                    total += n;
                }
                black_box(total);
            });
        });
    }
    group.finish();
}

/// Build-shaped macro bench. Models what `cargo check` does to a
/// source tree: enumerate one directory, then for each file open +
/// fully read it. We do 50 source-ish files (≈ what a small crate
/// has) plus a few rlib-sized blobs that get re-read across calls
/// (the dep-graph pattern). Two passes — cold (cache cleared) and
/// warm — to expose the cache's contribution end-to-end.
///
/// This is the macro signal closest to a real cargo build that
/// fits in a microbench: no kernel, no Cargo subprocess, no
/// toolchain dep. End-to-end FSKit/FUSE timing is a follow-up; the
/// numbers here bound the mount-core ceiling for the workflow.
fn bench_build_walk(c: &mut Criterion) {
    use std::ffi::OsStr;

    const SOURCE_COUNT: usize = 50;
    const SOURCE_SIZE: usize = 32 * 1024; // typical .rs
    const DEP_COUNT: usize = 4;
    const DEP_SIZE: usize = 2 * 1024 * 1024; // .rmeta-ish

    fn build_walk_fixture() -> (TempDir, ContentAddressedMount) {
        let temp = TempDir::new().expect("tempdir");
        let repo = Repository::init_default(temp.path()).expect("init repo");
        for i in 0..SOURCE_COUNT {
            let name = format!("src_{i:03}.rs");
            fs::write(temp.path().join(&name), fill(SOURCE_SIZE)).expect("write src");
        }
        for i in 0..DEP_COUNT {
            let name = format!("dep_{i}.rmeta");
            fs::write(temp.path().join(&name), fill(DEP_SIZE)).expect("write dep");
        }
        repo.snapshot(Some("build-walk fixture".into()), None)
            .expect("snapshot");
        let mount = ContentAddressedMount::new(repo, "main").expect("open mount");
        (temp, mount)
    }

    fn build_walk_vanilla() -> TempDir {
        let temp = TempDir::new().expect("tempdir");
        for i in 0..SOURCE_COUNT {
            let name = format!("src_{i:03}.rs");
            fs::write(temp.path().join(&name), fill(SOURCE_SIZE)).expect("write src");
        }
        for i in 0..DEP_COUNT {
            let name = format!("dep_{i}.rmeta");
            fs::write(temp.path().join(&name), fill(DEP_SIZE)).expect("write dep");
        }
        temp
    }

    let (_temp_repo, mount) = build_walk_fixture();
    let vanilla = build_walk_vanilla();

    let total_bytes = SOURCE_COUNT * SOURCE_SIZE + DEP_COUNT * DEP_SIZE;
    let mut group = c.benchmark_group("build_walk");
    group.throughput(Throughput::Bytes(total_bytes as u64));

    // Heddle warm: cache pre-populated by the bench itself after
    // the first sample. This is the steady-state cost of a repeat
    // build that re-touches the same files.
    let mut chunk = vec![0u8; 16 * 1024];
    group.bench_function("heddle_warm", |b| {
        b.iter(|| {
            let entries = mount.enumerate(NodeId::ROOT).expect("enumerate");
            for entry in &entries {
                let mut offset = 0u64;
                loop {
                    let n = mount
                        .read(entry.node, offset, black_box(&mut chunk))
                        .expect("heddle read");
                    if n == 0 {
                        break;
                    }
                    offset += n as u64;
                }
            }
            black_box(entries.len());
        });
    });

    // Heddle cold: clear the cache between iters so every file
    // pays the hydration cost. Bounds the cold-build case.
    group.bench_function("heddle_cold", |b| {
        b.iter(|| {
            mount.clear_blob_cache();
            let entries = mount.enumerate(NodeId::ROOT).expect("enumerate");
            for entry in &entries {
                let mut offset = 0u64;
                loop {
                    let n = mount
                        .read(entry.node, offset, black_box(&mut chunk))
                        .expect("heddle read");
                    if n == 0 {
                        break;
                    }
                    offset += n as u64;
                }
            }
            black_box(entries.len());
        });
    });

    // Vanilla baseline: readdir + open + read every file. Host
    // page cache is warm after the first iter, so this is the
    // best-case `cargo check` against unchanged files.
    let dir = vanilla.path().to_path_buf();
    group.bench_function("vanilla", |b| {
        b.iter(|| {
            let mut paths: Vec<PathBuf> = Vec::new();
            for entry in fs::read_dir(&dir).expect("readdir") {
                let entry = entry.expect("entry");
                if entry
                    .path()
                    .extension()
                    .is_some_and(|e| e == OsStr::new("rs") || e == OsStr::new("rmeta"))
                {
                    paths.push(entry.path());
                }
            }
            for path in &paths {
                let mut f = fs::File::open(path).expect("vanilla open");
                loop {
                    let n = f.read(black_box(&mut chunk)).expect("vanilla read");
                    if n == 0 {
                        break;
                    }
                }
            }
            black_box(paths.len());
        });
    });
    group.finish();
}

/// Decompose the cold-cache `read` path into the stages it actually
/// executes. Each row in the report is the average per-iteration
/// cost of that stage in isolation, so we can attribute the cold-
/// path budget concretely:
///
///   `read_uncompressed_file`   — vanilla `std::fs::File::open` + `read_to_end`
///                                of the SAME bytes the mount serves.
///                                Establishes the bar we're trying to beat.
///   `read_compressed_file`     — same syscalls but reading the smaller
///                                on-disk blob (no decompression). Bound for
///                                "I/O only" of the cold path.
///   `decompress_only`          — zstd decompress of already-loaded
///                                compressed bytes. The CPU half of the cold
///                                path.
///   `store_get_blob`           — `ObjectStore::get_blob` end-to-end (the
///                                composition of open + read + decompress).
///   `load_blob_bytes_cold`     — `ContentAddressedMount::read` after
///                                `clear_blob_cache()` (full mount cold path
///                                including Arc wrap + cache insert).
///
/// Numbers comparing the same blob across all rows are the apples-to-
/// apples picture we need to pick the right optimization.
fn bench_cold_stages(c: &mut Criterion) {
    // We need:
    //   * A heddle repo so `mount.read` works (cold path end-to-end).
    //   * Pre-compressed bytes for each tier, plus that same payload
    //     written to a sibling file on disk so we can isolate
    //     "read compressed bytes from disk" vs "decompress in memory".
    // `Repository::snapshot` writes blobs into a packfile, so we
    // can't point a bench loop at a loose blob path. We sidestep
    // that by calling `compress(...)` ourselves on each tier and
    // writing the result alongside the vanilla fixture.
    let (_temp_repo, mount) = build_mount_fixture();
    let blobs_tmp = TempDir::new().expect("blobs tmp");
    let cfg = CompressionConfig::default();

    struct Resolved {
        tier: Tier,
        node: NodeId,
        /// `None` when `compress` decided the input wasn't worth
        /// compressing for this size (skipped by zstd's heuristic).
        /// The `decompress_only` and `read_compressed_file` rows
        /// are omitted in that case — there's no compressed payload
        /// to bench against.
        compressed: Option<(Vec<u8>, PathBuf)>,
    }
    let resolved: Vec<Resolved> = TIERS
        .iter()
        .map(|tier| {
            let node = mount.lookup_path(tier.path).expect("lookup");
            let raw = fill(tier.size);
            let compressed = compress(&raw, &cfg).expect("compress").map(|bytes| {
                let path = blobs_tmp.path().join(format!("{}.zst", tier.name));
                fs::write(&path, &bytes).expect("write compressed");
                (bytes, path)
            });
            Resolved {
                tier: *tier,
                node,
                compressed,
            }
        })
        .collect();

    // Vanilla side: same bytes laid out on disk.
    let vanilla = build_vanilla_fixture();

    let mut group = c.benchmark_group("cold_stages");
    for r in &resolved {
        group.throughput(Throughput::Bytes(r.tier.size as u64));

        let vanilla_path = vanilla.path().join(r.tier.path);
        group.bench_with_input(
            BenchmarkId::new("read_uncompressed_file", r.tier.name),
            &vanilla_path,
            |b, path| {
                b.iter(|| {
                    let bytes = fs::read(path).expect("read uncompressed");
                    black_box(bytes.len());
                });
            },
        );

        if let Some((compressed_bytes, compressed_path)) = &r.compressed {
            group.bench_with_input(
                BenchmarkId::new("read_compressed_file", r.tier.name),
                compressed_path,
                |b, path| {
                    b.iter(|| {
                        let bytes = fs::read(path).expect("read compressed");
                        black_box(bytes.len());
                    });
                },
            );

            group.bench_with_input(
                BenchmarkId::new("decompress_only", r.tier.name),
                compressed_bytes,
                |b, compressed| {
                    b.iter(|| {
                        let out = decompress(black_box(compressed)).expect("decompress");
                        black_box(out.len());
                    });
                },
            );
        }

        group.bench_with_input(
            BenchmarkId::new("mount_read_cold", r.tier.name),
            &r.node,
            |b, node| {
                let mut buf = vec![0u8; r.tier.size];
                b.iter(|| {
                    mount.clear_blob_cache();
                    let n = mount.read(*node, 0, black_box(&mut buf)).expect("read");
                    black_box(n);
                });
            },
        );
    }
    group.finish();
}

/// Prewarm bench: model the daemon workflow where the mount kicks
/// off a background hydrator the moment it opens, then the agent
/// hits the mount slightly later. The two metrics we care about:
///
///   `prewarm_then_read` — total time from "open mount" through
///     "prewarm finished" + read every file. This bounds the wall-
///     clock when the agent races the prewarmer.
///   `read_after_prewarm` — time to read every file after the
///     prewarmer has completed. This is the steady-state cost
///     the agent actually sees if their first read happens after
///     the prewarm window.
///   `fork_warm_inherit` — open mount A, prewarm it, drop it,
///     then open mount B against the same Repository + the same
///     `Arc<BlobCachePool>`, and read all of B's files. Measures
///     the fork-thread case: mount B reuses A's warm bytes for
///     free, paying only walk-the-tree cost.
fn bench_prewarm(c: &mut Criterion) {
    const SOURCE_COUNT: usize = 50;
    const SOURCE_SIZE: usize = 32 * 1024;
    const DEP_COUNT: usize = 4;
    const DEP_SIZE: usize = 2 * 1024 * 1024;
    let total_bytes = SOURCE_COUNT * SOURCE_SIZE + DEP_COUNT * DEP_SIZE;

    fn fixture() -> (TempDir, Repository) {
        let temp = TempDir::new().expect("tempdir");
        let repo = Repository::init_default(temp.path()).expect("init repo");
        for i in 0..SOURCE_COUNT {
            fs::write(
                temp.path().join(format!("src_{i:03}.rs")),
                fill(SOURCE_SIZE),
            )
            .expect("write src");
        }
        for i in 0..DEP_COUNT {
            fs::write(temp.path().join(format!("dep_{i}.rmeta")), fill(DEP_SIZE))
                .expect("write dep");
        }
        repo.snapshot(Some("prewarm fixture".into()), None)
            .expect("snapshot");
        (temp, repo)
    }

    let mut group = c.benchmark_group("prewarm");
    group.throughput(Throughput::Bytes(total_bytes as u64));
    // Each iter rebuilds the mount + clears caches, so reduce sample
    // count to keep total bench time reasonable.
    group.sample_size(15);

    let mut chunk = vec![0u8; 16 * 1024];

    // 1. Read-all-files end-to-end with prewarm in-flight. Worst
    //    case: agent races the prewarmer; some files hit warm, some
    //    cold, depending on race outcomes.
    group.bench_function("prewarm_then_read", |b| {
        let (_temp, repo) = fixture();
        let pool = Arc::new(BlobCachePool::with_default_capacity());
        let opts = MountOptions {
            blob_cache: Some(Arc::clone(&pool)),
        };
        b.iter(|| {
            pool.clear();
            let mount = ContentAddressedMount::with_options(
                Repository::open(repo.root()).unwrap(),
                "main",
                opts.clone(),
            )
            .expect("open mount")
            .with_promotion_policy(no_sweep());
            let handle = mount.prewarm();
            let entries = mount.enumerate(NodeId::ROOT).expect("enum");
            for entry in &entries {
                let mut offset = 0u64;
                loop {
                    let n = mount
                        .read(entry.node, offset, black_box(&mut chunk))
                        .expect("read");
                    if n == 0 {
                        break;
                    }
                    offset += n as u64;
                }
            }
            // Wait for prewarmer too so we charge the full cost.
            let _stats = handle.wait();
        });
    });

    // 2. Read-all-files AFTER prewarm completed. This is the
    //    realistic steady-state for an agent whose first FS access
    //    happens >100ms after thread start.
    group.bench_function("read_after_prewarm", |b| {
        let (_temp, repo) = fixture();
        let pool = Arc::new(BlobCachePool::with_default_capacity());
        let opts = MountOptions {
            blob_cache: Some(Arc::clone(&pool)),
        };
        // Build the mount + prewarm ONCE outside the timed loop so
        // we measure the steady-state hot-cache read, not the
        // prewarm cost itself.
        let mount = ContentAddressedMount::with_options(
            Repository::open(repo.root()).unwrap(),
            "main",
            opts.clone(),
        )
        .expect("open mount");
        let _stats = mount.prewarm().wait();
        b.iter(|| {
            let entries = mount.enumerate(NodeId::ROOT).expect("enum");
            for entry in &entries {
                let mut offset = 0u64;
                loop {
                    let n = mount
                        .read(entry.node, offset, black_box(&mut chunk))
                        .expect("read");
                    if n == 0 {
                        break;
                    }
                    offset += n as u64;
                }
            }
        });
    });

    // 3. Fork-thread case: pool stays warm across mount drops.
    //    Setup (outside timed loop): open mount A, prewarm + drop.
    //    Timed: open mount B against the same shared pool, read all
    //    files. We're measuring "spin up a sibling and serve its
    //    first reads" — which is exactly the workflow when one
    //    daemon hosts multiple threads forked from a common parent.
    group.bench_function("fork_warm_inherit", |b| {
        let (_temp, repo) = fixture();
        let pool = Arc::new(BlobCachePool::with_default_capacity());
        // One-time: prewarm into the shared pool via a throwaway
        // mount, so subsequent mounts inherit a fully-warm cache.
        {
            let warmer = ContentAddressedMount::with_options(
                Repository::open(repo.root()).unwrap(),
                "main",
                MountOptions {
                    blob_cache: Some(Arc::clone(&pool)),
                },
            )
            .expect("open warmer mount")
            .with_promotion_policy(no_sweep());
            let _ = warmer.prewarm().wait();
        }
        b.iter(|| {
            let mount = ContentAddressedMount::with_options(
                Repository::open(repo.root()).unwrap(),
                "main",
                MountOptions {
                    blob_cache: Some(Arc::clone(&pool)),
                },
            )
            .expect("open mount B")
            .with_promotion_policy(no_sweep());
            let entries = mount.enumerate(NodeId::ROOT).expect("enum");
            for entry in &entries {
                let mut offset = 0u64;
                loop {
                    let n = mount
                        .read(entry.node, offset, black_box(&mut chunk))
                        .expect("read");
                    if n == 0 {
                        break;
                    }
                    offset += n as u64;
                }
            }
        });
    });

    // 4. Cold baseline: same fixture, fresh per-mount pool, no
    //    prewarm — what a CLI one-shot pays. Establishes how much
    //    work the prewarmer is actually saving the user.
    group.bench_function("cold_no_prewarm", |b| {
        let (_temp, repo) = fixture();
        b.iter(|| {
            let mount = ContentAddressedMount::new(Repository::open(repo.root()).unwrap(), "main")
                .expect("open mount")
                .with_promotion_policy(no_sweep());
            let entries = mount.enumerate(NodeId::ROOT).expect("enum");
            for entry in &entries {
                let mut offset = 0u64;
                loop {
                    let n = mount
                        .read(entry.node, offset, black_box(&mut chunk))
                        .expect("read");
                    if n == 0 {
                        break;
                    }
                    offset += n as u64;
                }
            }
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_full_read,
    bench_chunked_read,
    bench_chunked_read_cold,
    bench_mmap_pattern,
    bench_enumerate_then_attrs,
    bench_build_walk,
    bench_cold_stages,
    bench_prewarm,
);
criterion_main!(benches);
