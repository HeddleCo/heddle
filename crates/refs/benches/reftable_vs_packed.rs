// SPDX-License-Identifier: Apache-2.0
//! Spike bench (HeddleCo/heddle#21): packed-refs vs prototype reftable.
//!
//! Compares the two on-disk ref formats at 10k / 50k / 100k ref counts.
//! All sizes are dominated by threads (typical repo shape); markers stay zero
//! here because the per-section code paths are identical and adding a second
//! axis would obscure the comparison.
//!
//! What's measured:
//!
//! - `cold_load`: load the whole file from disk into an in-memory model
//!   (the unavoidable per-process cost).
//! - `cold_single_lookup`: open the file fresh, find one ref by name. For
//!   packed-refs this is a full parse + hashmap lookup; for reftable it's a
//!   binary-search over the offset index.
//! - `warm_lookup_x1000`: with the model already loaded, look up 1000 random
//!   refs (packed-refs hashmap shines here; reftable pays log N seeks per
//!   lookup).
//! - `list_all`: enumerate every ref name (used by `heddle status`, sync, etc.).
//! - `append_one_persist`: add one new ref and rewrite the whole file —
//!   including the actual filesystem write, `fsync`, and atomic rename via
//!   `objects::fs_atomic::write_file_atomic` (the same call `PackedRefs::save`
//!   uses in production). Both branches go through this path so the
//!   comparison reflects real-world rewrite latency, not just in-memory
//!   serialization cost.
//!
//! The bench also prints on-disk byte sizes for both formats at each scale,
//! as a "Throughput::Bytes" entry on the `cold_load` group.
//!
//! Run: `cargo bench -p heddle-refs --bench reftable_vs_packed`

use std::{
    fs,
    hint::black_box,
    path::{Path, PathBuf},
};

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use objects::fs_atomic::write_file_atomic;
use objects::object::ChangeId;
use refs::{PackedRefsModel, ReftableModel};
use tempfile::TempDir;

const SIZES: &[usize] = &[10_000, 50_000, 100_000];

/// Build a deterministic ChangeId from an index; spreads bytes across the
/// 16-byte payload so we don't accidentally pick a degenerate shape.
fn cid_for(i: usize) -> ChangeId {
    let mut bytes = [0u8; 16];
    let raw = (i as u128).wrapping_mul(0x9E37_79B9_7F4A_7C15_9E37_79B9_7F4A_7C15);
    bytes.copy_from_slice(&raw.to_le_bytes());
    ChangeId::from_bytes(bytes)
}

fn name_for(i: usize) -> String {
    // Mimic real branch shapes: "feature/<5-digit>", "topic/<5-digit>", etc.
    let bucket = i % 5;
    let prefix = ["feature", "topic", "release", "user/alice", "user/bob"][bucket];
    format!("{prefix}/branch-{i:06}")
}

fn build_packed(n: usize) -> PackedRefsModel {
    let mut m = PackedRefsModel::new();
    for i in 0..n {
        m.set_thread(&name_for(i), cid_for(i));
    }
    m
}

fn build_reftable(n: usize) -> ReftableModel {
    let mut m = ReftableModel::new();
    for i in 0..n {
        m.set_thread(&name_for(i), cid_for(i));
    }
    m
}

fn write_packed(path: &Path, model: &PackedRefsModel) {
    fs::write(path, model.to_text()).unwrap();
}

fn write_reftable(path: &Path, model: &ReftableModel) {
    fs::write(path, model.to_bytes()).unwrap();
}

fn lookup_names(n: usize, sample: usize) -> Vec<String> {
    // Spread across the keyspace with a fixed stride so the sample is
    // reproducible and doesn't cluster on a hot region.
    let stride = (n / sample).max(1);
    (0..sample).map(|i| name_for((i * stride) % n)).collect()
}

struct Layout {
    _dir: TempDir,
    packed_path: PathBuf,
    reftable_path: PathBuf,
}

fn lay_out(n: usize) -> Layout {
    let dir = TempDir::new().unwrap();
    let packed_path = dir.path().join("packed-refs");
    let reftable_path = dir.path().join("reftable");
    write_packed(&packed_path, &build_packed(n));
    write_reftable(&reftable_path, &build_reftable(n));
    Layout {
        _dir: dir,
        packed_path,
        reftable_path,
    }
}

fn bench_cold_load(c: &mut Criterion) {
    let mut g = c.benchmark_group("cold_load");
    for &n in SIZES {
        let layout = lay_out(n);
        let packed_bytes = fs::metadata(&layout.packed_path).unwrap().len();
        let reftable_bytes = fs::metadata(&layout.reftable_path).unwrap().len();
        eprintln!(
            "[size] n={n}  packed={packed_bytes} B  reftable={reftable_bytes} B  ratio={:.2}",
            reftable_bytes as f64 / packed_bytes as f64
        );

        g.throughput(Throughput::Bytes(packed_bytes));
        g.bench_with_input(BenchmarkId::new("packed", n), &layout, |b, layout| {
            b.iter(|| {
                let text = fs::read_to_string(&layout.packed_path).unwrap();
                let m = PackedRefsModel::parse(&text);
                black_box(m.list_threads().len());
            });
        });

        g.throughput(Throughput::Bytes(reftable_bytes));
        g.bench_with_input(BenchmarkId::new("reftable", n), &layout, |b, layout| {
            b.iter(|| {
                let bytes = fs::read(&layout.reftable_path).unwrap();
                let m = ReftableModel::from_bytes(&bytes).unwrap();
                black_box(m.thread_count());
            });
        });
    }
    g.finish();
}

fn bench_cold_single_lookup(c: &mut Criterion) {
    let mut g = c.benchmark_group("cold_single_lookup");
    for &n in SIZES {
        let layout = lay_out(n);
        let needle = name_for(n / 2);

        g.bench_with_input(BenchmarkId::new("packed", n), &layout, |b, layout| {
            b.iter(|| {
                let text = fs::read_to_string(&layout.packed_path).unwrap();
                let m = PackedRefsModel::parse(&text);
                black_box(m.get_thread(&needle));
            });
        });

        g.bench_with_input(BenchmarkId::new("reftable", n), &layout, |b, layout| {
            b.iter(|| {
                let bytes = fs::read(&layout.reftable_path).unwrap();
                black_box(ReftableModel::lookup_thread_in_bytes(&bytes, &needle).unwrap());
            });
        });
    }
    g.finish();
}

fn bench_warm_lookup(c: &mut Criterion) {
    const SAMPLE: usize = 1000;
    let mut g = c.benchmark_group("warm_lookup_x1000");
    for &n in SIZES {
        let packed = build_packed(n);
        let reftable = build_reftable(n);
        let needles = lookup_names(n, SAMPLE);

        g.bench_with_input(BenchmarkId::new("packed", n), &needles, |b, needles| {
            b.iter(|| {
                for name in needles {
                    black_box(packed.get_thread(name));
                }
            });
        });

        g.bench_with_input(BenchmarkId::new("reftable", n), &needles, |b, needles| {
            b.iter(|| {
                for name in needles {
                    black_box(reftable.get_thread(name));
                }
            });
        });
    }
    g.finish();
}

fn bench_list_all(c: &mut Criterion) {
    let mut g = c.benchmark_group("list_all");
    for &n in SIZES {
        let packed = build_packed(n);
        let reftable = build_reftable(n);

        g.bench_with_input(BenchmarkId::new("packed", n), &packed, |b, m| {
            b.iter(|| black_box(m.list_threads().len()));
        });

        g.bench_with_input(BenchmarkId::new("reftable", n), &reftable, |b, m| {
            b.iter(|| black_box(m.list_threads().len()));
        });
    }
    g.finish();
}

fn bench_append_one_persist(c: &mut Criterion) {
    let mut g = c.benchmark_group("append_one_persist");
    for &n in SIZES {
        let layout = lay_out(n);
        let new_name = format!("feature/new-{n:06}");
        let new_id = cid_for(n + 1);

        g.bench_with_input(BenchmarkId::new("packed", n), &layout, |b, layout| {
            b.iter_batched(
                || {
                    let text = fs::read_to_string(&layout.packed_path).unwrap();
                    PackedRefsModel::parse(&text)
                },
                |mut m| {
                    m.set_thread(&new_name, new_id);
                    let text = m.to_text();
                    write_file_atomic(&layout.packed_path, text.as_bytes()).unwrap();
                    black_box(&layout.packed_path);
                },
                BatchSize::SmallInput,
            );
        });

        g.bench_with_input(BenchmarkId::new("reftable", n), &layout, |b, layout| {
            b.iter_batched(
                || {
                    let bytes = fs::read(&layout.reftable_path).unwrap();
                    ReftableModel::from_bytes(&bytes).unwrap()
                },
                |mut m| {
                    m.set_thread(&new_name, new_id);
                    let bytes = m.to_bytes();
                    write_file_atomic(&layout.reftable_path, &bytes).unwrap();
                    black_box(&layout.reftable_path);
                },
                BatchSize::SmallInput,
            );
        });
    }
    g.finish();
}

criterion_group!(
    benches,
    bench_cold_load,
    bench_cold_single_lookup,
    bench_warm_lookup,
    bench_list_all,
    bench_append_one_persist,
);
criterion_main!(benches);
