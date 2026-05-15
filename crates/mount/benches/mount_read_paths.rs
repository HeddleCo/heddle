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
};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use mount::{ContentAddressedMount, NodeId, PlatformShell};
use repo::Repository;
use tempfile::TempDir;

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

criterion_group!(
    benches,
    bench_full_read,
    bench_chunked_read,
    bench_chunked_read_cold,
    bench_mmap_pattern,
    bench_enumerate_then_attrs,
);
criterion_main!(benches);
