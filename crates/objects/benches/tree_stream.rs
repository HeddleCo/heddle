// SPDX-License-Identifier: Apache-2.0
//! Wide-tree streaming benchmarks.
//!
//! Default size is 50_000 entries so CI-adjacent local runs finish quickly.
//! For the million-entry acceptance run:
//!
//! ```text
//! HEDDLE_WIDE_TREE_ENTRIES=1000000 cargo bench -p heddle-objects --bench tree_stream --features bench
//! ```
//!
//! Reports throughput, bytes reread after resume, and peak RSS (VmHWM).

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use objects::object::{
    BytesTreeSource, ContentHash, Tree, TreeByteSource, TreeEntry, TreeEntryReader, TreePageLimits,
};

fn wide_tree(entries: usize) -> Tree {
    Tree::from_entries(
        (0..entries)
            .map(|index| {
                TreeEntry::file(
                    format!("f{index:08}"),
                    ContentHash::compute(index.to_le_bytes().as_slice()),
                    false,
                )
                .expect("entry")
            })
            .collect(),
    )
}

fn peak_rss_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        let value = line.strip_prefix("VmHWM:")?;
        value.split_whitespace().next()?.parse().ok()
    })
}

fn stream_all(bytes: &[u8], tree_id: ContentHash, page: usize) {
    let mut reader = TreeEntryReader::open(
        BytesTreeSource::sequential_verify(bytes.to_vec()),
        tree_id,
        None,
    )
    .expect("open");
    let limits = TreePageLimits::new(page, usize::MAX).expect("limits");
    while let Some(chunk) = reader.next_page(limits).expect("page") {
        black_box(chunk.entries.len());
    }
    reader.finish_and_verify().expect("verify");
}

fn bench_tree_stream(c: &mut Criterion) {
    let entries = std::env::var("HEDDLE_WIDE_TREE_ENTRIES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(50_000);
    let tree = wide_tree(entries);
    let bytes = tree.encode_canonical().expect("encode");
    let tree_id = tree.hash();
    eprintln!(
        "wide-tree bench: entries={entries} encoded_bytes={} peak_rss_kb_before={:?}",
        bytes.len(),
        peak_rss_kb()
    );

    let mut group = c.benchmark_group("tree_stream");
    group.throughput(Throughput::Elements(entries as u64));
    group.bench_function(BenchmarkId::new("eager_decode", entries), |bencher| {
        bencher.iter(|| {
            black_box(Tree::decode_canonical(black_box(&bytes)).expect("decode"));
        });
    });
    group.bench_function(BenchmarkId::new("stream_pages_512", entries), |bencher| {
        bencher.iter(|| stream_all(black_box(&bytes), tree_id, 512));
    });
    group.finish();

    let mut reader = TreeEntryReader::open(
        BytesTreeSource::sequential_verify(bytes.clone()),
        tree_id,
        None,
    )
    .expect("open");
    let first = reader
        .next_page(TreePageLimits::new(512, usize::MAX).expect("limits"))
        .expect("page")
        .expect("some");
    let cursor = first.resume_cursor;
    drop(reader);

    struct Spy {
        inner: BytesTreeSource,
        reads: Vec<(u64, usize)>,
    }
    impl TreeByteSource for &mut Spy {
        fn read_exact_at(
            &mut self,
            offset: u64,
            buf: &mut [u8],
        ) -> Result<(), objects::object::TreeStreamError> {
            self.reads.push((offset, buf.len()));
            self.inner.read_exact_at(offset, buf)
        }
        fn len(&self) -> u64 {
            self.inner.len()
        }
        fn integrity(&self) -> objects::object::TreeBodyIntegrity {
            self.inner.integrity()
        }
        fn bytes_read(&self) -> u64 {
            self.inner.bytes_read()
        }
    }
    let mut spy = Spy {
        inner: BytesTreeSource::verified_placement(bytes.clone()),
        reads: Vec::new(),
    };
    let mut resumed = TreeEntryReader::open(&mut spy, tree_id, Some(&cursor)).expect("resume");
    let limits = TreePageLimits::new(512, usize::MAX).expect("limits");
    while resumed.next_page(limits).expect("page").is_some() {}
    resumed.finish_and_verify().expect("verify");
    let prefix = cursor.byte_offset();
    let reread_prefix: u64 = spy
        .reads
        .iter()
        .filter(|(offset, _)| *offset > 0 && *offset < prefix)
        .map(|(_, len)| *len as u64)
        .sum();
    eprintln!(
        "wide-tree resume: prefix_end={prefix} bytes_read={} prefix_bytes_reread={reread_prefix} peak_rss_kb_after={:?}",
        resumed.bytes_read(),
        peak_rss_kb()
    );
}

criterion_group!(benches, bench_tree_stream);
criterion_main!(benches);
