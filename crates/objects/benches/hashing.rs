// SPDX-License-Identifier: Apache-2.0
//! Direct throughput benchmarks for typed content hashing.
//!
//! Run: `cargo bench -p heddle-objects --bench hashing`

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use objects::object::ContentHash;

const SIZES: &[usize] = &[1024, 64 * 1024, 1024 * 1024, 16 * 1024 * 1024];

fn deterministic_bytes(len: usize) -> Vec<u8> {
    let mut state = 0x1234_5678_9abc_def0u64 ^ len as u64;
    let mut bytes = Vec::with_capacity(len);
    while bytes.len() < len {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    bytes.truncate(len);
    bytes
}

fn bench_compute_typed(c: &mut Criterion) {
    let mut group = c.benchmark_group("hashing_compute_typed");
    for &size in SIZES {
        let data = deterministic_bytes(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &data, |b, data| {
            b.iter(|| {
                let hash = ContentHash::compute_typed("blob", black_box(data));
                black_box(hash);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_compute_typed);
criterion_main!(benches);
