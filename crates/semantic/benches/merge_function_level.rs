// SPDX-License-Identifier: Apache-2.0
//! Throughput + cost-split benchmarks for the function-level merge driver.
//!
//! Three measurement axes:
//!
//! 1. **File size**: small (100 lines), medium (1k lines), large (10k lines).
//! 2. **Engine**: `text_hunk_merge` (the historical baseline) vs
//!    `semantic_three_way_merge` (the new driver). The contrast tells us how
//!    much the AST pass costs *and* what we get for it.
//! 3. **Workload shape**: disjoint-function edits (best case for semantic)
//!    vs structural reshape (worst case for text_hunk_merge).
//!
//! Heddle's bench convention (see `crates/cli/benches/local_ops.rs`) is to
//! exercise both the cold and warm cache when caching is meaningful. The
//! semantic driver has no cross-call cache today (each invocation parses
//! fresh); we still report two variants for the same workload to expose
//! per-iteration variance.
//!
//! Baseline JSON: criterion writes its baseline files into the workspace
//! target/criterion dir. Use `cargo bench --save-baseline pre-68` /
//! `--save-baseline post-68` to capture before/after numbers and compare
//! with `cargo bench --baseline pre-68`.

use std::{hint::black_box, path::Path};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use merge::{ConflictMarkers, text_hunk_merge_with_markers};
use semantic::semantic_three_way_merge;

const MARKERS: ConflictMarkers<'static> = ConflictMarkers {
    ours: "OURS",
    theirs: "THEIRS",
};

/// Build a synthetic Rust source file with `n` simple functions.
fn synth_file(n: usize, suffix: &str) -> String {
    let mut s = String::with_capacity(n * 60);
    for i in 0..n {
        s.push_str(&format!("fn fn_{i}() {{ let x = {i}{suffix}; }}\n\n"));
    }
    s
}

/// Disjoint-function shape: ours edits the first half, theirs edits the
/// second half. Both engines should produce a clean merge.
fn make_disjoint(n: usize) -> (String, String, String) {
    let base = synth_file(n, "");
    let mut ours = base.clone();
    let mut theirs = base.clone();
    for i in 0..(n / 2) {
        ours = ours.replace(
            &format!("fn fn_{i}() {{ let x = {i}; }}"),
            &format!("fn fn_{i}() {{ let x = {i}_OURS; }}"),
        );
    }
    for i in (n / 2)..n {
        theirs = theirs.replace(
            &format!("fn fn_{i}() {{ let x = {i}; }}"),
            &format!("fn fn_{i}() {{ let x = {i}_THEIRS; }}"),
        );
    }
    (base, ours, theirs)
}

/// Structural reshape shape: ours reorders all functions, theirs edits one.
/// text_hunk_merge struggles here; the semantic driver resolves cleanly.
fn make_structural_reshape(n: usize) -> (String, String, String) {
    let base = synth_file(n, "");
    // ours: reverse function order.
    let mut ours_lines: Vec<String> = base
        .split("\n\n")
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    ours_lines.reverse();
    let ours = ours_lines.join("\n\n") + "\n\n";

    // theirs: modify the middle function.
    let mid = n / 2;
    let theirs = base.replace(
        &format!("fn fn_{mid}() {{ let x = {mid}; }}"),
        &format!("fn fn_{mid}() {{ let x = {mid}_THEIRS; }}"),
    );
    (base, ours, theirs)
}

/// Single-file throughput across three sizes for both engines + both shapes.
fn bench_throughput(c: &mut Criterion) {
    let path = Path::new("synth.rs");
    let sizes: [usize; 3] = [10, 100, 1_000]; // 10 fns ≈ 100 lines, 1000 ≈ 10k lines
    let mut group = c.benchmark_group("merge_throughput_disjoint");
    for &n in &sizes {
        let (base, ours, theirs) = make_disjoint(n);
        let total_bytes = (base.len() + ours.len() + theirs.len()) as u64;
        group.throughput(Throughput::Bytes(total_bytes));
        group.bench_with_input(BenchmarkId::new("text_hunk", n), &n, |b, _| {
            b.iter(|| {
                let outcome = text_hunk_merge_with_markers(
                    black_box(base.as_bytes()),
                    black_box(ours.as_bytes()),
                    black_box(theirs.as_bytes()),
                    MARKERS,
                );
                black_box(outcome);
            });
        });
        group.bench_with_input(BenchmarkId::new("semantic", n), &n, |b, _| {
            b.iter(|| {
                let outcome = semantic_three_way_merge(
                    black_box(base.as_bytes()),
                    black_box(ours.as_bytes()),
                    black_box(theirs.as_bytes()),
                    path,
                    MARKERS,
                );
                black_box(outcome);
            });
        });
    }
    group.finish();

    let mut group = c.benchmark_group("merge_throughput_structural_reshape");
    for &n in &sizes {
        let (base, ours, theirs) = make_structural_reshape(n);
        let total_bytes = (base.len() + ours.len() + theirs.len()) as u64;
        group.throughput(Throughput::Bytes(total_bytes));
        group.bench_with_input(BenchmarkId::new("text_hunk", n), &n, |b, _| {
            b.iter(|| {
                let outcome = text_hunk_merge_with_markers(
                    black_box(base.as_bytes()),
                    black_box(ours.as_bytes()),
                    black_box(theirs.as_bytes()),
                    MARKERS,
                );
                black_box(outcome);
            });
        });
        group.bench_with_input(BenchmarkId::new("semantic", n), &n, |b, _| {
            b.iter(|| {
                let outcome = semantic_three_way_merge(
                    black_box(base.as_bytes()),
                    black_box(ours.as_bytes()),
                    black_box(theirs.as_bytes()),
                    path,
                    MARKERS,
                );
                black_box(outcome);
            });
        });
    }
    group.finish();
}

/// Cost-split: parse-only vs full merge, so we can attribute parser overhead
/// independently from the merge logic.
fn bench_cost_split(c: &mut Criterion) {
    use semantic::{Language, ParsedFile};
    let mut group = c.benchmark_group("merge_cost_split_1000_fns");
    let (base, ours, theirs) = make_disjoint(1_000);
    let path = Path::new("synth.rs");
    let lang = Language::Rust;

    group.bench_function("parse_only_x3", |b| {
        b.iter(|| {
            let _ = black_box(ParsedFile::parse(black_box(base.as_str()), lang));
            let _ = black_box(ParsedFile::parse(black_box(ours.as_str()), lang));
            let _ = black_box(ParsedFile::parse(black_box(theirs.as_str()), lang));
        });
    });
    group.bench_function("full_semantic_merge", |b| {
        b.iter(|| {
            let outcome = semantic_three_way_merge(
                black_box(base.as_bytes()),
                black_box(ours.as_bytes()),
                black_box(theirs.as_bytes()),
                path,
                MARKERS,
            );
            black_box(outcome);
        });
    });
    group.bench_function("text_hunk_baseline", |b| {
        b.iter(|| {
            let outcome = text_hunk_merge_with_markers(
                black_box(base.as_bytes()),
                black_box(ours.as_bytes()),
                black_box(theirs.as_bytes()),
                MARKERS,
            );
            black_box(outcome);
        });
    });
    group.finish();
}

criterion_group!(benches, bench_throughput, bench_cost_split);
criterion_main!(benches);
