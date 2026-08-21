// SPDX-License-Identifier: Apache-2.0
//! Large-file LCS and deep-history blame-slice benches.
//!
//! ```text
//! cargo bench -p heddle-objects --bench line_diff_blame --features bench,memory-backend
//! ```
//!
//! Peak scratch comes from the returned [`ResourceUsage`]. Cancellation
//! latency is the time until the visitor returns `Err` on the first equal run.

use std::{hint::black_box, path::Path, time::Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use objects::{
    blame::{
        BlamePreparation, BlameSliceAdvance, BlameSliceLimits, advance_file_blame_slice,
        prepare_file_blame,
    },
    object::{Attribution, Blob, Principal, State, Tree, TreeEntry},
    store::{InMemoryStore, ObjectStore},
    util::{LineDiffLimits, ResourceUsage, scratch_bytes_for_line_counts, visit_lcs_equal_runs},
};

fn minified_lcs(c: &mut Criterion) {
    let old = (0..8_000)
        .map(|i| format!("var x{i}=1;"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut new = old.clone();
    new.replace_range(0..12, "var x0=2;");
    let needed = scratch_bytes_for_line_counts(8_000, 8_000);
    let mut scratch = vec![0u8; needed];

    c.bench_function("minified_equal_run_lcs", |b| {
        b.iter(|| {
            let mut budget = LineDiffLimits::unlimited().budget(scratch.len());
            let usage = visit_lcs_equal_runs(
                black_box(old.as_bytes()),
                black_box(new.as_bytes()),
                &mut scratch,
                &mut budget,
                |_| Ok::<(), std::convert::Infallible>(()),
            )
            .expect("lcs");
            black_box(usage)
        });
    });

    let started = Instant::now();
    let mut cancel_budget = LineDiffLimits::unlimited().budget(scratch.len());
    let _ = visit_lcs_equal_runs(
        old.as_bytes(),
        new.as_bytes(),
        &mut scratch,
        &mut cancel_budget,
        |_| Err("cancel"),
    );
    let cancel_ms = started.elapsed().as_secs_f64() * 1000.0;
    let mut usage_budget = LineDiffLimits::unlimited().budget(scratch.len());
    let usage: ResourceUsage = visit_lcs_equal_runs(
        old.as_bytes(),
        new.as_bytes(),
        &mut scratch,
        &mut usage_budget,
        |_| Ok::<(), std::convert::Infallible>(()),
    )
    .expect("usage");
    eprintln!(
        "minified LCS scratch_bytes={} work={} cancel_ms={cancel_ms:.3} rss_kb={}",
        usage.scratch_bytes,
        usage.work,
        peak_rss_kb().unwrap_or(0)
    );
}

fn peak_rss_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("VmHWM:") {
            let kb = value.split_whitespace().next()?;
            return kb.parse().ok();
        }
    }
    None
}

fn deep_history_slices(c: &mut Criterion) {
    let store = InMemoryStore::new();
    let mut parent = None;
    let mut tip = None;
    for index in 0..400 {
        let body = format!("keep\nline {index}\n");
        let blob_hash = store.put_blob(&Blob::from_slice(body.as_bytes())).unwrap();
        let tree_hash = store
            .put_tree(&Tree::from_entries(vec![
                TreeEntry::file("lib.rs".to_string(), blob_hash, false).unwrap(),
            ]))
            .unwrap();
        let state = State::new(
            tree_hash,
            parent.map(|id| vec![id]).unwrap_or_default(),
            Attribution::human(Principal::new("bench", "bench@example.com")),
        );
        store.put_state(&state).unwrap();
        parent = Some(state.id());
        tip = Some(state);
    }
    let tip = tip.expect("history");
    let path = Path::new("lib.rs");
    let limits = BlameSliceLimits {
        states: 4,
        decoded_bytes: 64 * 1024,
        lines: 64,
        diff_work: 4_096,
        scratch_bytes: 64 * 1024,
    };

    c.bench_function("deep_history_blame_slices", |b| {
        b.iter(|| {
            let BlamePreparation::Active { mut frontier, .. } =
                prepare_file_blame(&store, &tip, path, limits).expect("prepare")
            else {
                panic!("active");
            };
            let mut slices = 0u32;
            loop {
                match advance_file_blame_slice(&store, path, frontier, limits).expect("slice") {
                    BlameSliceAdvance::Progress { next, usage, .. } => {
                        assert!(usage.states <= limits.states);
                        assert!(usage.decoded_bytes <= limits.decoded_bytes);
                        assert!(usage.lines > 0);
                        frontier = next;
                        slices += 1;
                    }
                    BlameSliceAdvance::Complete { usage, .. } => {
                        assert!(usage.states <= limits.states);
                        black_box(slices);
                        break;
                    }
                }
            }
        });
    });
}

criterion_group!(benches, minified_lcs, deep_history_slices);
criterion_main!(benches);
