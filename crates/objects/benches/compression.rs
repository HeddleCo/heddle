// SPDX-License-Identifier: Apache-2.0
//! zstd compression/decompression throughput benchmarks.
//!
//! Run: `cargo bench -p heddle-objects --features bench,zstd --bench compression`

use std::hint::black_box;

#[cfg(feature = "zstd")]
use criterion::{BenchmarkId, Throughput};
use criterion::{Criterion, criterion_group, criterion_main};
#[cfg(feature = "zstd")]
use heddle_format::compression::{compress_zstd, decompress_zstd};

#[cfg(feature = "zstd")]
const LEVELS: &[i32] = &[1, 3, 19];
#[cfg(feature = "zstd")]
const CORPUS_SIZE: usize = 1024 * 1024;

#[cfg(feature = "zstd")]
fn representative_blob_corpus() -> Vec<u8> {
    let mut out = Vec::with_capacity(CORPUS_SIZE);
    let patterns = [
        b"fn main() { println!(\"heddle object store benchmark\"); }\n".as_slice(),
        b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n".as_slice(),
        b"\x00\x01\x02\x03\x05\x08\x0d\x15\x22\x37\x59\x90\xea".as_slice(),
    ];
    let mut state = 0xa5a5_5a5a_dead_beefu64;
    while out.len() < CORPUS_SIZE {
        state = state
            .wrapping_mul(2862933555777941757)
            .wrapping_add(3037000493);
        let selector = (state as usize) % patterns.len();
        out.extend_from_slice(patterns[selector]);
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(CORPUS_SIZE);
    out
}

fn bench_zstd(c: &mut Criterion) {
    #[cfg(not(feature = "zstd"))]
    {
        let mut group = c.benchmark_group("compression_zstd_unavailable");
        group.bench_function("feature_disabled", |b| {
            b.iter(|| black_box(()));
        });
        group.finish();
    }

    #[cfg(feature = "zstd")]
    {
        let corpus = representative_blob_corpus();

        let mut compress_group = c.benchmark_group("compression_zstd_compress");
        for &level in LEVELS {
            compress_group.throughput(Throughput::Bytes(corpus.len() as u64));
            compress_group.bench_with_input(
                BenchmarkId::new("level", level),
                &level,
                |b, &level| {
                    b.iter(|| {
                        let compressed =
                            compress_zstd(black_box(&corpus), black_box(level)).unwrap();
                        black_box(compressed);
                    });
                },
            );
        }
        compress_group.finish();

        let compressed_by_level: Vec<(i32, Vec<u8>)> = LEVELS
            .iter()
            .copied()
            .map(|level| {
                let compressed = compress_zstd(&corpus, level).unwrap();
                eprintln!(
                    "[ratio] level={level} raw={} compressed={} ratio={:.3}",
                    corpus.len(),
                    compressed.len(),
                    compressed.len() as f64 / corpus.len() as f64
                );
                (level, compressed)
            })
            .collect();

        let mut decompress_group = c.benchmark_group("compression_zstd_decompress");
        for (level, compressed) in &compressed_by_level {
            decompress_group.throughput(Throughput::Bytes(corpus.len() as u64));
            decompress_group.bench_with_input(
                BenchmarkId::new("level", level),
                compressed,
                |b, compressed| {
                    b.iter(|| {
                        let decompressed =
                            decompress_zstd(black_box(compressed), black_box(corpus.len() as u64))
                                .unwrap();
                        black_box(decompressed);
                    });
                },
            );
        }
        decompress_group.finish();
    }
}

criterion_group!(benches, bench_zstd);
criterion_main!(benches);
