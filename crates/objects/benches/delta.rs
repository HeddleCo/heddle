// SPDX-License-Identifier: Apache-2.0
//! Delta encoder/decoder throughput benchmarks.
//!
//! Run: `cargo bench -p heddle-objects --bench delta`

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use heddle_format::delta::{DeltaDecoder, DeltaEncoder, MAX_DELTA_OUTPUT_SIZE};

const SIZES: &[usize] = &[64 * 1024, 1024 * 1024];

fn base_blob(size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(size);
    for i in 0..size {
        out.push(((i.wrapping_mul(31) ^ (i >> 7)) & 0xff) as u8);
    }
    out
}

fn target_blob(base: &[u8]) -> Vec<u8> {
    let mut target = base.to_vec();
    for i in (0..target.len()).step_by(4096) {
        let end = (i + 64).min(target.len());
        for (j, byte) in target[i..end].iter_mut().enumerate() {
            *byte = byte
                .wrapping_add((j as u8).wrapping_mul(17))
                .wrapping_add(3);
        }
    }
    target
}

fn bench_delta(c: &mut Criterion) {
    let mut encode_group = c.benchmark_group("delta_encode");
    for &size in SIZES {
        let base = base_blob(size);
        let target = target_blob(&base);
        encode_group.throughput(Throughput::Bytes(target.len() as u64));
        encode_group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            b.iter(|| {
                let delta = DeltaEncoder::encode(black_box(&base), black_box(&target));
                black_box(delta);
            });
        });
    }
    encode_group.finish();

    let mut decode_group = c.benchmark_group("delta_decode");
    for &size in SIZES {
        let base = base_blob(size);
        let target = target_blob(&base);
        let delta = DeltaEncoder::encode(&base, &target);
        decode_group.throughput(Throughput::Bytes(target.len() as u64));
        decode_group.bench_with_input(BenchmarkId::from_parameter(size), &delta, |b, delta| {
            b.iter(|| {
                let decoded =
                    DeltaDecoder::decode(black_box(&base), black_box(delta), MAX_DELTA_OUTPUT_SIZE)
                        .unwrap();
                black_box(decoded);
            });
        });
    }
    decode_group.finish();
}

criterion_group!(benches, bench_delta);
criterion_main!(benches);
