// SPDX-License-Identifier: Apache-2.0
//! Pack reader and streaming pack build/install benchmarks.
//!
//! Run: `cargo bench -p heddle-objects --bench pack_io`

use std::{fs::OpenOptions, hint::black_box};

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use heddle_format::compression::CompressionConfig;
use objects::{
    object::ContentHash,
    store::{
        FsStore, ObjectStore,
        pack::{ObjectType, PackBuilder, PackObjectId, PackReader, StreamingPackBuilder},
    },
};
use tempfile::TempDir;

const OBJECT_COUNT: usize = 2_048;
const BLOB_SIZE: usize = 8 * 1024;
const DELTA_DEPTHS: &[usize] = &[0, 8];

struct PackFixture {
    pack_data: Vec<u8>,
    index_data: Vec<u8>,
    ids: Vec<PackObjectId>,
    total_bytes: u64,
}

fn compression_for_bench() -> CompressionConfig {
    CompressionConfig {
        enabled: false,
        level: 0,
        min_size: usize::MAX,
        max_delta_size: 10 * 1024 * 1024,
    }
}

fn blob_bytes(index: usize, size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(size);
    for i in 0..size {
        out.push(((index * 131 + i * 17 + (i >> 5)) & 0xff) as u8);
    }
    out
}

fn versioned_blob(version: usize, size: usize) -> Vec<u8> {
    let mut out = blob_bytes(7, size);
    for i in 0..=version {
        let pos = (i * 4099) % out.len();
        out[pos] = out[pos].wrapping_add((version as u8).wrapping_add(1));
    }
    out
}

fn raw_pack_fixture() -> PackFixture {
    let mut builder = PackBuilder::new(CompressionConfig::disabled());
    let mut ids = Vec::with_capacity(OBJECT_COUNT);
    let mut total_bytes = 0;
    for i in 0..OBJECT_COUNT {
        let data = blob_bytes(i, BLOB_SIZE);
        let hash = ContentHash::compute_typed("blob", &data);
        ids.push(PackObjectId::Hash(hash));
        total_bytes += data.len() as u64;
        builder.add(hash, ObjectType::Blob, data);
    }
    let (pack_data, index_data, _stats) = builder.build().unwrap();
    PackFixture {
        pack_data,
        index_data,
        ids,
        total_bytes,
    }
}

fn delta_pack_fixture(depth: usize) -> PackFixture {
    let mut builder = PackBuilder::new(compression_for_bench());
    let mut ids = Vec::with_capacity(depth + 1);
    let mut total_bytes = 0;
    for version in 0..=depth {
        let data = versioned_blob(version, BLOB_SIZE);
        let hash = ContentHash::compute_typed("blob", &data);
        ids.push(PackObjectId::Hash(hash));
        total_bytes += data.len() as u64;
        builder.add_with_path(
            hash,
            ObjectType::Blob,
            data,
            Some("src/versioned.rs".to_string()),
        );
    }
    let (pack_data, index_data, stats) = builder.build().unwrap();
    eprintln!(
        "[pack] delta-depth={depth} objects={} deltas={} ratio={:.3}",
        stats.object_count, stats.delta_count, stats.compression_ratio
    );
    PackFixture {
        pack_data,
        index_data,
        ids,
        total_bytes,
    }
}

fn streaming_pack_layout() -> (TempDir, std::path::PathBuf, std::path::PathBuf, u64) {
    let dir = TempDir::new().unwrap();
    let pack_path = dir.path().join("stream.pack");
    let index_path = dir.path().join("stream.idx");
    let bucket_dir = dir.path().join("buckets");
    let pack_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&pack_path)
        .unwrap();
    let mut builder = StreamingPackBuilder::new(
        pack_file,
        index_path.clone(),
        CompressionConfig::disabled(),
        bucket_dir,
    )
    .unwrap();

    let mut total_bytes = 0;
    for i in 0..OBJECT_COUNT {
        let data = blob_bytes(i, BLOB_SIZE);
        let hash = ContentHash::compute_typed("blob", &data);
        total_bytes += data.len() as u64;
        builder.add(hash, ObjectType::Blob, data).unwrap();
    }
    let (_file, _stats) = builder.finalize().unwrap();
    (dir, pack_path, index_path, total_bytes)
}

fn bench_pack_reads(c: &mut Criterion) {
    let raw = raw_pack_fixture();
    let reader = PackReader::from_bytes(raw.pack_data.clone(), &raw.index_data).unwrap();
    let sample_ids: Vec<PackObjectId> = raw.ids.iter().step_by(32).copied().collect();

    let mut warm = c.benchmark_group("pack_io_get_object_bytes_warm");
    warm.throughput(Throughput::Bytes((sample_ids.len() * BLOB_SIZE) as u64));
    warm.bench_with_input(
        BenchmarkId::new("raw", sample_ids.len()),
        &sample_ids,
        |b, ids| {
            b.iter(|| {
                for id in ids {
                    let (_ty, bytes) = reader.get_object_bytes(black_box(id)).unwrap().unwrap();
                    black_box(bytes);
                }
            });
        },
    );
    warm.finish();

    let mut cold = c.benchmark_group("pack_io_get_object_bytes_cold");
    cold.throughput(Throughput::Bytes(BLOB_SIZE as u64));
    let cold_id = raw.ids[raw.ids.len() / 2];
    cold.bench_function("raw_reopen", |b| {
        b.iter_batched(
            || PackReader::from_bytes(raw.pack_data.clone(), &raw.index_data).unwrap(),
            |reader| {
                let (_ty, bytes) = reader
                    .get_object_bytes(black_box(&cold_id))
                    .unwrap()
                    .unwrap();
                black_box(bytes);
            },
            BatchSize::SmallInput,
        );
    });
    cold.finish();

    let mut delta = c.benchmark_group("pack_io_get_object_bytes_delta_chain");
    for &depth in DELTA_DEPTHS {
        let fixture = if depth == 0 {
            raw_pack_fixture()
        } else {
            delta_pack_fixture(depth)
        };
        let reader =
            PackReader::from_bytes(fixture.pack_data.clone(), &fixture.index_data).unwrap();
        let id = *fixture.ids.last().unwrap();
        delta.throughput(Throughput::Bytes(BLOB_SIZE as u64));
        delta.bench_with_input(BenchmarkId::from_parameter(depth), &id, |b, id| {
            b.iter(|| {
                let (_ty, bytes) = reader.get_object_bytes(black_box(id)).unwrap().unwrap();
                black_box(bytes);
            });
        });
    }
    delta.finish();
}

fn bench_streaming_build_install(c: &mut Criterion) {
    let mut group = c.benchmark_group("pack_io_streaming_build_install");
    group.throughput(Throughput::Bytes((OBJECT_COUNT * BLOB_SIZE) as u64));
    group.bench_function(BenchmarkId::from_parameter(OBJECT_COUNT), |b| {
        b.iter_batched(
            streaming_pack_layout,
            |(_layout_dir, pack_path, index_path, total_bytes)| {
                let store_dir = TempDir::new().unwrap();
                let store = FsStore::new(store_dir.path());
                store.init().unwrap();
                store
                    .install_pack_streaming(black_box(&pack_path), black_box(&index_path))
                    .unwrap();
                black_box(total_bytes);
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

fn bench_pack_build_in_memory(c: &mut Criterion) {
    let mut group = c.benchmark_group("pack_io_packbuilder_build");
    group.throughput(Throughput::Bytes((OBJECT_COUNT * BLOB_SIZE) as u64));
    group.bench_function(BenchmarkId::from_parameter(OBJECT_COUNT), |b| {
        b.iter(|| {
            let fixture = raw_pack_fixture();
            black_box((fixture.pack_data, fixture.index_data, fixture.total_bytes));
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_pack_reads,
    bench_pack_build_in_memory,
    bench_streaming_build_install
);
criterion_main!(benches);
