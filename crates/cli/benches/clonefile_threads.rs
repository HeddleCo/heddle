// SPDX-License-Identifier: Apache-2.0
//! Performance bench for the clonefile-thread surface
//! (`docs/design/clonefile-threads.md`).
//!
//! Three scenarios per file-count, with the file count varying across
//! 1k / 10k / 100k:
//!
//! 1. `materialize_cold` — `Repository::materialize_thread` from CAS
//!    into a fresh TempDir. The full clonefile/copy + manifest write
//!    cycle. This is what `heddle thread start --workspace
//!    materialized` does on the first start.
//! 2. `capture_noop_fast_path` — `Repository::capture_thread_from_disk`
//!    on a freshly-materialised worktree with no edits. Exercises the
//!    stat-cache fast no-op (`stat_cache_no_op` in
//!    `repository_thread_materialize.rs`); should bottom out at a
//!    `stat`-per-file scan.
//! 3. `capture_single_edit` — same, after editing one file. Exercises
//!    the stat-cache *slow* path (one read+hash for the changed file,
//!    stat-cache reuse for the rest). This is the "agent touched a
//!    couple of files, capturing now" pattern.
//!
//! Defaults to 1k and 10k. Set `HEDDLE_BENCH_LARGE_SYNTHETIC=100000`
//! (or any integer > 10_000) to include a 100k pass. The 100k pass
//! is opt-in because fixture setup alone takes tens of seconds on
//! cold filesystem caches.
//!
//! Run with:
//!
//! ```sh
//! cargo bench -p heddle-cli --bench clonefile_threads
//! HEDDLE_BENCH_LARGE_SYNTHETIC=100000 \
//!     cargo bench -p heddle-cli --bench clonefile_threads
//! ```

use std::{env, hint::black_box, path::Path};

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use repo::Repository;
use tempfile::TempDir;

/// File-count axis. 1k and 10k always; 100k under env gate so CI runs
/// stay cheap and developers can opt-in locally.
fn synthetic_file_counts() -> Vec<usize> {
    let mut counts = vec![1_000, 10_000];
    if let Ok(value) = env::var("HEDDLE_BENCH_LARGE_SYNTHETIC")
        && let Ok(parsed) = value.parse::<usize>()
        && parsed > 10_000
    {
        counts.push(parsed);
    }
    counts
}

/// Write `count` small files spread across 20 directories. Mirrors the
/// `even_spread` shape in `local_ops.rs` so per-count numbers stay
/// comparable to existing tree-walk benches.
fn write_synthetic_files(root: &Path, count: usize) {
    for i in 0..count {
        let dir = root.join(format!("dir-{:02}", i % 20));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("file-{i:05}.txt")),
            format!("synthetic file {i}\n{}\n", "x".repeat(64)),
        )
        .unwrap();
    }
}

/// Stand up a repo with `count` files captured on `main`. Returns the
/// repo wrapped in its owning TempDir so the caller controls when the
/// fixture is dropped.
fn fixture_repo_with_files(count: usize) -> (TempDir, Repository) {
    let dir = TempDir::new().unwrap();
    let repo = Repository::init_default(dir.path()).unwrap();
    write_synthetic_files(dir.path(), count);
    repo.snapshot(Some(format!("seed {count} files")), None)
        .unwrap();
    (dir, repo)
}

fn bench_materialize_cold(c: &mut Criterion) {
    let mut group = c.benchmark_group("clonefile_threads/materialize_cold");
    for &count in &synthetic_file_counts() {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            let (_fixture, repo) = fixture_repo_with_files(count);
            // `iter_batched_ref` so the destination TempDir's
            // recursive `unlink` of `count` materialised files
            // happens outside the timed region. Without this, the
            // 10k+ variants were dominated by destructor noise
            // rather than the materialize cost itself.
            b.iter_batched_ref(
                || TempDir::new().unwrap(),
                |dest| {
                    let path = dest.path().join("out");
                    let manifest = repo
                        .materialize_thread("main", &path, &repo::AudienceTier::Internal)
                        .expect("materialize");
                    black_box(manifest);
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

fn bench_capture_noop_fast_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("clonefile_threads/capture_noop_fast_path");
    for &count in &synthetic_file_counts() {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            // One fresh materialise per iteration: stat-cache no-op
            // is the post-materialise state we want to measure.
            //
            // Use `iter_batched_ref` (not `iter_batched`): the
            // closure borrows `&mut input` instead of consuming it,
            // so the recursive TempDir drop for `count` files
            // happens *outside* the timed region. With
            // `iter_batched` the per-iteration cost at 10k files
            // was ≈98% TempDir destructor noise (≈2 s of
            // recursive `unlink`) drowning the ≈90 ms actual
            // routine — measurements were unusable.
            b.iter_batched_ref(
                || {
                    let (fixture, repo) = fixture_repo_with_files(count);
                    let dest = TempDir::new().unwrap();
                    let dest_path = dest.path().join("out");
                    repo.materialize_thread("main", &dest_path, &repo::AudienceTier::Internal)
                        .unwrap();
                    (fixture, repo, dest, dest_path)
                },
                |(_fixture, repo, _dest, dest_path)| {
                    let outcome = repo
                        .capture_thread_from_disk("main", dest_path)
                        .expect("capture");
                    black_box(outcome);
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

fn bench_capture_single_edit(c: &mut Criterion) {
    let mut group = c.benchmark_group("clonefile_threads/capture_single_edit");
    for &count in &synthetic_file_counts() {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter_batched_ref(
                || {
                    let (fixture, repo) = fixture_repo_with_files(count);
                    let dest = TempDir::new().unwrap();
                    let dest_path = dest.path().join("out");
                    repo.materialize_thread("main", &dest_path, &repo::AudienceTier::Internal)
                        .unwrap();
                    // Edit a single file (the file in dir-00) so the
                    // stat-cache invalidates exactly one entry and
                    // the slow path runs minimally — the "agent
                    // touched one file" scenario.
                    let edited = dest_path.join("dir-00").join("file-00000.txt");
                    std::fs::write(&edited, b"edited\n").unwrap();
                    (fixture, repo, dest, dest_path)
                },
                |(_fixture, repo, _dest, dest_path)| {
                    let outcome = repo
                        .capture_thread_from_disk("main", dest_path)
                        .expect("capture");
                    black_box(outcome);
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

criterion_group!(
    clonefile_threads,
    bench_materialize_cold,
    bench_capture_noop_fast_path,
    bench_capture_single_edit,
);
criterion_main!(clonefile_threads);
