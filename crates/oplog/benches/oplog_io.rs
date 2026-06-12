// SPDX-License-Identifier: Apache-2.0
//! Oplog v3 hot-path I/O benchmarks.
//!
//! These benches intentionally build current-format on-disk oplogs and then
//! measure the production `OpLog` paths over them:
//!
//! - append one operation over 100 / 10k / 100k existing entries
//! - indexed read latency for `head_id`, `recent(N)`, and `last`
//! - exact-once transaction dedup and #392 CAS commit paths over a large
//!   transaction directory
//!
//! Run: `cargo bench -p heddle-oplog --features bench --bench oplog_io`
//! (the `bench` feature is required — the bench links feature-gated shims).

use std::{
    collections::BTreeSet,
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::{TimeZone, Utc};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use objects::object::{ChangeId, Principal};
use oplog::{
    ConditionalCommitOutcome, IsolationKey, IsolationPrecondition, OpEntry, OpLog, OpLogBackend,
    OpRecord,
};
use tempfile::TempDir;

const APPEND_SIZES: &[usize] = &[100, 10_000, 100_000];
const LARGE_LOG_SIZE: usize = 100_000;
const RECENT_COUNT: usize = 100;
const SEED_BATCH_WIDTH: usize = 100;
const SEED: u128 = 0x4260_4230_3920_0000_0123_4567_89ab_cdef;

struct Fixture {
    _dir: TempDir,
    root: PathBuf,
}

impl Fixture {
    fn clone_to_temp(&self) -> Fixture {
        let dir = TempDir::new().unwrap();
        copy_dir(self.root.join("oplog"), dir.path().join("oplog"));
        fs::create_dir_all(dir.path().join("locks")).unwrap();
        Fixture {
            root: dir.path().to_path_buf(),
            _dir: dir,
        }
    }

    fn log(&self) -> OpLog {
        open_log(&self.root)
    }
}

fn open_log(root: &Path) -> OpLog {
    OpLog::new(root, Principal::new("bench", "bench@example.com"))
}

fn bench_actor() -> Arc<Principal> {
    Arc::new(Principal::new("bench", "bench@example.com"))
}

fn copy_dir(from: impl AsRef<Path>, to: impl AsRef<Path>) {
    fs::create_dir_all(to.as_ref()).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let src = entry.path();
        let dst = to.as_ref().join(entry.file_name());
        if src.is_dir() {
            copy_dir(src, dst);
        } else {
            fs::copy(src, dst).unwrap();
        }
    }
}

fn cid_for(i: usize) -> ChangeId {
    let raw = SEED
        .wrapping_add(i as u128)
        .wrapping_mul(0x9e37_79b9_7f4a_7c15_6eed_0e9d_a4d9_4a4f);
    ChangeId::from_bytes(raw.to_le_bytes())
}

fn thread_name(i: usize) -> String {
    format!("thread-{i:06}")
}

fn update_record(i: usize) -> OpRecord {
    OpRecord::ThreadUpdate {
        name: thread_name(i),
        old_state: cid_for(i),
        new_state: cid_for(i + 1),
        old_manager_snapshot: None,
        new_manager_snapshot: None,
    }
}

fn commit_record(transaction_id: impl Into<String>, op_count: u32) -> OpRecord {
    OpRecord::TransactionCommit {
        transaction_id: transaction_id.into(),
        op_count,
    }
}

fn exact_once_operations(transaction_id: &str, i: usize) -> Vec<OpRecord> {
    vec![update_record(i), commit_record(transaction_id, 1)]
}

fn entry_for(index: usize, operation: OpRecord) -> OpEntry {
    let id = index as u64 + 1;
    let batch_start = ((index / SEED_BATCH_WIDTH) * SEED_BATCH_WIDTH) as u64 + 1;
    OpEntry {
        id,
        timestamp: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
        operation,
        undone: false,
        batch_id: batch_start,
        batch_index: (index % SEED_BATCH_WIDTH) as u32,
        scope: None,
        actor: bench_actor(),
        operation_id: None,
    }
}

fn cas_precondition(
    since_head_id: u64,
    keys: impl IntoIterator<Item = IsolationKey>,
) -> IsolationPrecondition {
    IsolationPrecondition {
        since_head_id,
        keys: keys.into_iter().collect::<BTreeSet<_>>(),
    }
}

fn seed_fixture(entries: Vec<OpEntry>) -> Fixture {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    let log = open_log(&root);
    log.write_entries_for_bench(entries).unwrap();
    Fixture { _dir: dir, root }
}

fn append_fixture(existing_entries: usize) -> Fixture {
    seed_fixture(
        (0..existing_entries)
            .map(|i| entry_for(i, update_record(i)))
            .collect(),
    )
}

fn indexed_read_fixture() -> Fixture {
    append_fixture(LARGE_LOG_SIZE)
}

fn transaction_fixture() -> Fixture {
    let entries = (0..LARGE_LOG_SIZE)
        .map(|i| entry_for(i, commit_record(format!("tx-{i:06}"), 0)))
        .collect();
    seed_fixture(entries)
}

fn bench_append_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("oplog_append_throughput");
    group.sample_size(10);
    group.throughput(Throughput::Elements(1));

    for &existing in APPEND_SIZES {
        let fixture = append_fixture(existing);
        group.bench_with_input(
            BenchmarkId::new("record_snapshot", existing),
            &fixture,
            |b, fixture| {
                b.iter_batched(
                    || fixture.clone_to_temp(),
                    |layout| {
                        let log = layout.log();
                        let id = log
                            .record_snapshot(
                                &cid_for(existing + 1),
                                Some(&cid_for(existing)),
                                None,
                                None,
                            )
                            .unwrap();
                        black_box(id);
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

fn bench_indexed_reads(c: &mut Criterion) {
    let fixture = indexed_read_fixture();
    let log = fixture.log();

    let mut group = c.benchmark_group("oplog_indexed_reads");
    group.sample_size(10);

    group.bench_function(BenchmarkId::new("read_head_id", LARGE_LOG_SIZE), |b| {
        b.iter(|| black_box(log.read_head_id_for_bench().unwrap()));
    });

    group.throughput(Throughput::Elements(RECENT_COUNT as u64));
    group.bench_function(BenchmarkId::new("recent", LARGE_LOG_SIZE), |b| {
        b.iter(|| black_box(log.recent(RECENT_COUNT).unwrap()));
    });

    group.throughput(Throughput::Elements(1));
    group.bench_function(BenchmarkId::new("last", LARGE_LOG_SIZE), |b| {
        b.iter(|| black_box(log.last().unwrap()));
    });

    group.finish();
}

fn bench_exactly_once(c: &mut Criterion) {
    let fixture = transaction_fixture();
    let committed_tx = format!("tx-{:06}", LARGE_LOG_SIZE / 2);

    let mut group = c.benchmark_group("oplog_exactly_once");
    group.sample_size(10);
    group.throughput(Throughput::Elements(1));

    group.bench_function(BenchmarkId::new("dedup_hit", LARGE_LOG_SIZE), |b| {
        b.iter(|| {
            let log = fixture.log();
            let result = log
                .record_batch_exactly_once(
                    exact_once_operations(&committed_tx, LARGE_LOG_SIZE + 1),
                    None,
                    &committed_tx,
                )
                .unwrap();
            black_box(result);
        });
    });

    group.bench_with_input(
        BenchmarkId::new("commit_miss", LARGE_LOG_SIZE),
        &fixture,
        |b, fixture| {
            b.iter_batched(
                || fixture.clone_to_temp(),
                |layout| {
                    let log = layout.log();
                    let result = log
                        .record_batch_exactly_once(
                            exact_once_operations("tx-new-exact-once", LARGE_LOG_SIZE + 2),
                            None,
                            "tx-new-exact-once",
                        )
                        .unwrap();
                    black_box(result);
                },
                BatchSize::SmallInput,
            );
        },
    );

    group.finish();
}

fn bench_exactly_once_cas(c: &mut Criterion) {
    let transaction_fixture = transaction_fixture();
    let contention_fixture = indexed_read_fixture();
    let committed_tx = format!("tx-{:06}", LARGE_LOG_SIZE - 1);
    let current_head = transaction_fixture.log().head_id().unwrap();
    let no_change = cas_precondition(current_head, []);
    let conflicting_thread = thread_name(LARGE_LOG_SIZE - 1);
    let contention = cas_precondition(
        contention_fixture.log().head_id().unwrap() - 100,
        [IsolationKey::Thread(conflicting_thread)],
    );

    let mut group = c.benchmark_group("oplog_exactly_once_cas");
    group.sample_size(10);
    group.throughput(Throughput::Elements(1));

    group.bench_function(BenchmarkId::new("dedup_hit", LARGE_LOG_SIZE), |b| {
        b.iter(|| {
            let log = transaction_fixture.log();
            let outcome = log
                .record_batch_exactly_once_if_unchanged(
                    exact_once_operations(&committed_tx, LARGE_LOG_SIZE + 3),
                    None,
                    &committed_tx,
                    &no_change,
                )
                .unwrap();
            black_box(outcome);
        });
    });

    group.bench_with_input(
        BenchmarkId::new("commit_no_change", LARGE_LOG_SIZE),
        &transaction_fixture,
        |b, fixture| {
            b.iter_batched(
                || fixture.clone_to_temp(),
                |layout| {
                    let log = layout.log();
                    let outcome = log
                        .record_batch_exactly_once_if_unchanged(
                            exact_once_operations("tx-new-cas", LARGE_LOG_SIZE + 4),
                            None,
                            "tx-new-cas",
                            &no_change,
                        )
                        .unwrap();
                    black_box(outcome);
                },
                BatchSize::SmallInput,
            );
        },
    );

    group.bench_function(
        BenchmarkId::new("same_thread_contention", LARGE_LOG_SIZE),
        |b| {
            b.iter(|| {
                let log = contention_fixture.log();
                let outcome = log
                    .record_batch_exactly_once_if_unchanged(
                        exact_once_operations("tx-conflicting-cas", LARGE_LOG_SIZE + 5),
                        None,
                        "tx-conflicting-cas",
                        &contention,
                    )
                    .unwrap();
                if !matches!(outcome, ConditionalCommitOutcome::IsolationConflict { .. }) {
                    panic!("expected CAS isolation conflict");
                }
                black_box(outcome);
            });
        },
    );

    group.finish();
}

criterion_group!(
    benches,
    bench_append_throughput,
    bench_indexed_reads,
    bench_exactly_once,
    bench_exactly_once_cas,
);
criterion_main!(benches);
