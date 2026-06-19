// SPDX-License-Identifier: Apache-2.0
use std::{env, hint::black_box, path::Path};

use cli::bench::{detect_renames_for_bench, find_merge_base_for_bench, three_way_merge_for_bench};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use objects::{
    object::{Blob, ChangeId, MarkerName, ThreadName, Tree, TreeEntry},
    store::{BlockingObjectStore, InMemoryStore},
};
use refs::{Head, RefExpectation, RefManager};
use repo::{
    FsMonitorMode, FsMonitorSettings, Repository, WorktreeStatusOptions, run_local_monitor_helper,
};
use semantic::{
    SemanticDiffOptions, SemanticParseCache, SimilarityMethod, semantic_check_only, semantic_diff,
    semantic_diff_summary,
};
use tempfile::TempDir;

#[derive(Clone, Copy, Debug)]
enum SyntheticShape {
    EvenSpread,
    Wide,
    Deep,
    ManySmallDirectories,
    FewHugeDirectories,
}

impl SyntheticShape {
    fn name(self) -> &'static str {
        match self {
            Self::EvenSpread => "even_spread",
            Self::Wide => "wide",
            Self::Deep => "deep",
            Self::ManySmallDirectories => "many_small_dirs",
            Self::FewHugeDirectories => "few_huge_dirs",
        }
    }
}

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

fn write_files(root: &Path, count: usize, prefix: &str) {
    write_shape(root, count, prefix, SyntheticShape::EvenSpread);
}

fn write_flat_files(root: &Path, count: usize) {
    for i in 0..count {
        std::fs::write(
            root.join(format!("file_{i:05}.txt")),
            format!("flat file {i}\n{}\n", "x".repeat(64)),
        )
        .unwrap();
    }
}

fn write_shape(root: &Path, count: usize, prefix: &str, shape: SyntheticShape) {
    match shape {
        SyntheticShape::EvenSpread => {
            for i in 0..count {
                let dir = root.join(format!("{prefix}/dir-{:02}", i % 20));
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join(format!("file-{i:05}.txt")),
                    format!("{prefix} file {i}\n{}\n", "x".repeat(64)),
                )
                .unwrap();
            }
        }
        SyntheticShape::Wide => {
            for i in 0..count {
                let dir = root.join(format!("{prefix}/dir-{i:05}"));
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join("entry.txt"),
                    format!("wide file {i}\n{}\n", "x".repeat(64)),
                )
                .unwrap();
            }
        }
        SyntheticShape::Deep => {
            for i in 0..count {
                let dir = root.join(prefix).join(format!(
                    "l0-{:02}/l1-{:02}/l2-{:02}/l3-{:02}/l4-{:02}/l5-{:02}",
                    i % 8,
                    (i / 8) % 8,
                    (i / 64) % 8,
                    (i / 512) % 8,
                    (i / 4_096) % 8,
                    (i / 32_768) % 8,
                ));
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join(format!("file-{i:05}.txt")),
                    format!("deep file {i}\n{}\n", "x".repeat(64)),
                )
                .unwrap();
            }
        }
        SyntheticShape::ManySmallDirectories => {
            for i in 0..count {
                let dir = root.join(format!("{prefix}/shard-{:03}/item-{i:05}", i % 256));
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join("entry.txt"),
                    format!("small dir file {i}\n{}\n", "x".repeat(64)),
                )
                .unwrap();
            }
        }
        SyntheticShape::FewHugeDirectories => {
            for i in 0..count {
                let dir = root.join(format!("{prefix}/bucket-{:02}", i % 4));
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join(format!("file-{i:05}.txt")),
                    format!("huge dir file {i}\n{}\n", "x".repeat(64)),
                )
                .unwrap();
            }
        }
    }
}

fn setup_repo_with_files(count: usize) -> (TempDir, Repository) {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    write_files(temp.path(), count, "tracked");
    (temp, repo)
}

fn setup_flat_repo_with_files(count: usize) -> (TempDir, Repository) {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    write_flat_files(temp.path(), count);
    (temp, repo)
}

fn setup_repo_with_shape(shape: SyntheticShape, count: usize) -> (TempDir, Repository) {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    write_shape(temp.path(), count, "tracked", shape);
    (temp, repo)
}

fn setup_repo_with_many_untracked_dirs(
    tracked_count: usize,
    untracked_dir_count: usize,
) -> (TempDir, Repository) {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    write_files(temp.path(), tracked_count, "tracked");
    for i in 0..untracked_dir_count {
        let dir = temp
            .path()
            .join(format!("scratch/bucket-{:03}/dir-{i:05}", i % 128));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("note.txt"),
            format!("untracked dir {i}\n{}\n", "z".repeat(48)),
        )
        .unwrap();
    }
    (temp, repo)
}

fn ref_scale_counts() -> [usize; 4] {
    [1_000, 10_000, 50_000, 100_000]
}

fn setup_ref_manager() -> (TempDir, RefManager) {
    let temp = TempDir::new().unwrap();
    let heddle_dir = temp.path().join(".heddle");
    std::fs::create_dir_all(&heddle_dir).unwrap();
    let refs = RefManager::new(&heddle_dir);
    refs.init().unwrap();
    (temp, refs)
}

fn populate_threads(refs: &RefManager, count: usize) {
    for index in 0..count {
        refs.set_thread(
            &ThreadName::new(format!("branch-{index:05}")),
            &ChangeId::generate(),
        )
        .unwrap();
    }
}

fn populate_markers(refs: &RefManager, count: usize) {
    for index in 0..count {
        refs.create_marker(
            &MarkerName::new(format!("marker-{index:05}")),
            &ChangeId::generate(),
        )
        .unwrap();
    }
}

fn populate_remote_threads(refs: &RefManager, remote: &str, count: usize) {
    for index in 0..count {
        refs.set_remote_thread(
            remote,
            &ThreadName::new(format!("branch-{index:05}")),
            &ChangeId::generate(),
        )
        .unwrap();
    }
}

#[cfg(unix)]
fn setup_repo_with_symlinks(count: usize) -> (TempDir, Repository) {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let target_root = temp.path().join("targets");
    std::fs::create_dir_all(&target_root).unwrap();
    for i in 0..count {
        let target = target_root.join(format!("target-{i:05}.txt"));
        std::fs::write(&target, format!("target {i}\n")).unwrap();
        let link_dir = temp.path().join(format!("links/dir-{:02}", i % 32));
        std::fs::create_dir_all(&link_dir).unwrap();
        std::os::unix::fs::symlink(
            Path::new("..")
                .join("..")
                .join("targets")
                .join(format!("target-{i:05}.txt")),
            link_dir.join(format!("link-{i:05}")),
        )
        .unwrap();
    }
    (temp, repo)
}

fn bench_build_tree(c: &mut Criterion) {
    let mut group = c.benchmark_group("build_tree");
    for &file_count in &[100usize, 1_000] {
        let (_temp, repo) = setup_repo_with_files(file_count);
        group.bench_with_input(
            BenchmarkId::from_parameter(file_count),
            &file_count,
            |b, _| {
                b.iter(|| {
                    let tree = repo.build_tree(repo.root()).unwrap();
                    black_box(tree);
                });
            },
        );
    }
    group.finish();
}

fn bench_build_tree_cold_shapes(c: &mut Criterion) {
    let mut group = c.benchmark_group("build_tree_cold_shape");
    for shape in [
        SyntheticShape::Wide,
        SyntheticShape::Deep,
        SyntheticShape::ManySmallDirectories,
        SyntheticShape::FewHugeDirectories,
    ] {
        for file_count in synthetic_file_counts() {
            let (_temp, repo) = setup_repo_with_shape(shape, file_count);
            group.bench_with_input(
                BenchmarkId::new(shape.name(), file_count),
                &file_count,
                |b, _| {
                    b.iter(|| {
                        let tree = repo.build_tree(repo.root()).unwrap();
                        black_box(tree);
                    });
                },
            );
        }
    }
    group.finish();
}

fn bench_compare_worktree(c: &mut Criterion) {
    let mut group = c.benchmark_group("compare_worktree");
    for &file_count in &[100usize, 1_000] {
        let (_temp, repo) = setup_repo_with_files(file_count);
        let state = repo.snapshot(Some("base".to_string()), None).unwrap();
        let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();
        std::fs::write(
            repo.root().join("tracked/dir-00/file-00000.txt"),
            "modified\n",
        )
        .unwrap();

        group.bench_with_input(
            BenchmarkId::from_parameter(file_count),
            &file_count,
            |b, _| {
                b.iter(|| {
                    let status = repo.compare_worktree_cached(&tree).unwrap();
                    black_box(status.change_count());
                });
            },
        );
    }
    group.finish();
}

fn bench_snapshot_profile_flat_repo(c: &mut Criterion) {
    let mut group = c.benchmark_group("snapshot_profile_flat_repo");
    let file_count = 1_000usize;
    group.throughput(Throughput::Elements(file_count as u64));

    let sample = {
        let (_temp, repo) = setup_flat_repo_with_files(file_count);
        let attribution = repo.get_attribution().unwrap();
        repo.snapshot_with_attribution_profiled(Some("bench".to_string()), None, attribution)
            .unwrap()
            .profile
    };
    eprintln!(
        "snapshot_profile_flat_repo sample: files={} tree_walk_ms={} blob_prep_ms={} blob_write_ms={} tree_write_ms={} state_ref_oplog_ms={}",
        file_count,
        sample.tree_walk_ms,
        sample.blob_prep_ms,
        sample.blob_write_ms,
        sample.tree_write_ms,
        sample.state_ref_oplog_ms
    );

    group.bench_function(BenchmarkId::new("profiled_snapshot", file_count), |b| {
        b.iter_batched(
            || setup_flat_repo_with_files(file_count),
            |(_temp, repo)| {
                let attribution = repo.get_attribution().unwrap();
                let execution = repo
                    .snapshot_with_attribution_profiled(
                        Some("bench".to_string()),
                        None,
                        attribution,
                    )
                    .unwrap();
                black_box((
                    execution.state.change_id,
                    execution.profile.tree_walk_ms,
                    execution.profile.blob_prep_ms,
                    execution.profile.blob_write_ms,
                    execution.profile.tree_write_ms,
                    execution.profile.state_ref_oplog_ms,
                ));
            },
            BatchSize::PerIteration,
        );
    });

    group.finish();
}

fn bench_compare_worktree_many_added_files(c: &mut Criterion) {
    let mut group = c.benchmark_group("compare_worktree_many_added_files");
    for &added_count in &[1_000usize, 10_000] {
        let (_temp, repo) = setup_repo_with_files(1_000);
        let state = repo.snapshot(Some("base".to_string()), None).unwrap();
        let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();
        write_shape(
            repo.root(),
            added_count,
            "new-files",
            SyntheticShape::FewHugeDirectories,
        );

        group.bench_with_input(
            BenchmarkId::from_parameter(added_count),
            &added_count,
            |b, _| {
                b.iter(|| {
                    let status = repo.compare_worktree_cached(&tree).unwrap();
                    black_box((
                        status.added.len(),
                        status.modified.len(),
                        status.deleted.len(),
                    ));
                });
            },
        );
    }
    group.finish();
}

fn bench_compare_worktree_untracked_dirs_with_tracked_changes(c: &mut Criterion) {
    let mut group = c.benchmark_group("compare_worktree_untracked_dirs_with_tracked_changes");
    for &(tracked_count, untracked_dirs) in &[(1_000usize, 1_000usize), (1_000usize, 10_000usize)] {
        let (_temp, repo) = setup_repo_with_many_untracked_dirs(tracked_count, untracked_dirs);
        let state = repo.snapshot(Some("base".to_string()), None).unwrap();
        let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();

        for path in [
            "tracked/dir-00/file-00000.txt",
            "tracked/dir-01/file-00001.txt",
            "tracked/dir-02/file-00002.txt",
        ] {
            std::fs::write(repo.root().join(path), format!("edited {path}\n")).unwrap();
        }

        group.bench_with_input(
            BenchmarkId::new(
                format!("tracked_{tracked_count}_untracked_dirs_{untracked_dirs}"),
                tracked_count,
            ),
            &untracked_dirs,
            |b, _| {
                b.iter(|| {
                    let status = repo.compare_worktree_cached(&tree).unwrap();
                    black_box((status.added.len(), status.modified.len()));
                });
            },
        );
    }
    group.finish();
}

fn bench_compare_worktree_untracked_dirs_second_run(c: &mut Criterion) {
    let mut group = c.benchmark_group("compare_worktree_untracked_dirs_second_run");
    for &(tracked_count, untracked_dirs) in &[(1_000usize, 1_000usize), (1_000usize, 10_000usize)] {
        let (_temp, repo) = setup_repo_with_many_untracked_dirs(tracked_count, untracked_dirs);
        let state = repo.snapshot(Some("base".to_string()), None).unwrap();
        let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();

        for path in [
            "tracked/dir-00/file-00000.txt",
            "tracked/dir-01/file-00001.txt",
            "tracked/dir-02/file-00002.txt",
        ] {
            std::fs::write(repo.root().join(path), format!("edited {path}\n")).unwrap();
        }

        let warmup = repo.compare_worktree_cached(&tree).unwrap();
        black_box((warmup.added.len(), warmup.modified.len()));

        group.bench_with_input(
            BenchmarkId::new(
                format!("tracked_{tracked_count}_untracked_dirs_{untracked_dirs}"),
                tracked_count,
            ),
            &untracked_dirs,
            |b, _| {
                b.iter(|| {
                    let status = repo.compare_worktree_cached(&tree).unwrap();
                    black_box((status.added.len(), status.modified.len()));
                });
            },
        );
    }
    group.finish();
}

fn bench_refs_list_threads_summary(c: &mut Criterion) {
    let mut group = c.benchmark_group("refs_list_threads_summary");
    for count in ref_scale_counts() {
        let (_temp, refs) = setup_ref_manager();
        populate_threads(&refs, count);
        refs.rebuild_ref_summary_index().unwrap();
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
            b.iter(|| {
                black_box(refs.list_threads().unwrap().len());
            });
        });
    }
    group.finish();
}

fn bench_refs_list_markers_summary(c: &mut Criterion) {
    let mut group = c.benchmark_group("refs_list_markers_summary");
    for count in ref_scale_counts() {
        let (_temp, refs) = setup_ref_manager();
        populate_markers(&refs, count);
        refs.rebuild_ref_summary_index().unwrap();
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
            b.iter(|| {
                black_box(refs.list_markers().unwrap().len());
            });
        });
    }
    group.finish();
}

fn bench_refs_list_remote_threads_summary(c: &mut Criterion) {
    let mut group = c.benchmark_group("refs_list_remote_threads_summary");
    for count in ref_scale_counts() {
        let (_temp, refs) = setup_ref_manager();
        populate_remote_threads(&refs, "origin", count);
        refs.rebuild_ref_summary_index().unwrap();
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
            b.iter(|| {
                black_box(refs.list_remote_threads("origin").unwrap().len());
            });
        });
    }
    group.finish();
}

fn bench_refs_update_thread_rebuild_summary(c: &mut Criterion) {
    let mut group = c.benchmark_group("refs_update_thread_rebuild_summary");
    for count in ref_scale_counts() {
        let (_temp, refs) = setup_ref_manager();
        populate_threads(&refs, count);
        let hot = ThreadName::new(format!("branch-{:05}", count / 2));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
            b.iter(|| {
                refs.set_thread(&hot, &ChangeId::generate()).unwrap();
            });
        });
    }
    group.finish();
}

fn bench_refs_update_marker_rebuild_summary(c: &mut Criterion) {
    let mut group = c.benchmark_group("refs_update_marker_rebuild_summary");
    for count in ref_scale_counts() {
        let (_temp, refs) = setup_ref_manager();
        populate_markers(&refs, count);
        let hot = MarkerName::new(format!("marker-{:05}", count / 2));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
            b.iter(|| {
                refs.set_marker_cas(&hot, RefExpectation::Any, &ChangeId::generate())
                    .unwrap();
            });
        });
    }
    group.finish();
}

fn bench_refs_update_remote_thread_rebuild_summary(c: &mut Criterion) {
    let mut group = c.benchmark_group("refs_update_remote_thread_rebuild_summary");
    for count in ref_scale_counts() {
        let (_temp, refs) = setup_ref_manager();
        populate_remote_threads(&refs, "origin", count);
        let hot = ThreadName::new(format!("branch-{:05}", count / 2));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
            b.iter(|| {
                refs.set_remote_thread("origin", &hot, &ChangeId::generate())
                    .unwrap();
            });
        });
    }
    group.finish();
}

fn bench_worktree_is_clean_untracked_dirs_second_run(c: &mut Criterion) {
    let mut group = c.benchmark_group("worktree_is_clean_untracked_dirs_second_run");
    for &(tracked_count, untracked_dirs) in &[(1_000usize, 1_000usize), (1_000usize, 10_000usize)] {
        let (_temp, repo) = setup_repo_with_many_untracked_dirs(tracked_count, untracked_dirs);
        let state = repo.snapshot(Some("base".to_string()), None).unwrap();
        let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();

        let warmup = repo.worktree_is_clean_cached(&tree).unwrap();
        black_box(warmup);

        group.bench_with_input(
            BenchmarkId::new(
                format!("tracked_{tracked_count}_untracked_dirs_{untracked_dirs}"),
                tracked_count,
            ),
            &untracked_dirs,
            |b, _| {
                b.iter(|| {
                    let clean = repo.worktree_is_clean_cached(&tree).unwrap();
                    black_box(clean);
                });
            },
        );
    }
    group.finish();
}

#[cfg(unix)]
fn bench_compare_worktree_symlink_heavy(c: &mut Criterion) {
    let mut group = c.benchmark_group("compare_worktree_symlink_heavy");
    for &symlink_count in &[1_000usize, 10_000] {
        let (_temp, repo) = setup_repo_with_symlinks(symlink_count);
        let state = repo.snapshot(Some("base".to_string()), None).unwrap();
        let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();
        std::os::unix::fs::symlink(
            Path::new("..")
                .join("..")
                .join("targets")
                .join("target-00000.txt"),
            repo.root().join("links/dir-00/extra-link"),
        )
        .unwrap();

        group.bench_with_input(
            BenchmarkId::from_parameter(symlink_count),
            &symlink_count,
            |b, _| {
                b.iter(|| {
                    let status = repo.compare_worktree_cached(&tree).unwrap();
                    black_box((status.added.len(), status.modified.len()));
                });
            },
        );
    }
    group.finish();
}

fn bench_worktree_clean(c: &mut Criterion) {
    let mut group = c.benchmark_group("worktree_is_clean");
    for &file_count in &[100usize, 1_000] {
        let (_temp, repo) = setup_repo_with_files(file_count);
        let state = repo.snapshot(Some("base".to_string()), None).unwrap();
        let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();

        group.bench_with_input(
            BenchmarkId::from_parameter(file_count),
            &file_count,
            |b, _| {
                b.iter(|| {
                    let clean = repo.worktree_is_clean_cached(&tree).unwrap();
                    black_box(clean);
                });
            },
        );
    }
    group.finish();
}

fn bench_worktree_clean_modes(c: &mut Criterion) {
    let mut group = c.benchmark_group("worktree_is_clean_mode");
    let file_count = 1_000usize;
    let (_temp, repo) = setup_repo_with_files(file_count);
    let state = repo.snapshot(Some("base".to_string()), None).unwrap();
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();

    let off_options = WorktreeStatusOptions {
        fsmonitor: FsMonitorSettings {
            mode: FsMonitorMode::Off,
        },
    };
    group.bench_with_input(BenchmarkId::new("off", file_count), &file_count, |b, _| {
        b.iter(|| {
            let clean = repo
                .worktree_is_clean_cached_with_options(&tree, &off_options)
                .unwrap();
            black_box(clean);
        });
    });

    let native_options = WorktreeStatusOptions {
        fsmonitor: FsMonitorSettings {
            mode: FsMonitorMode::Native,
        },
    };
    let root = repo.root().to_path_buf();
    std::thread::spawn(move || {
        let _ = run_local_monitor_helper(&root);
    });
    std::thread::sleep(std::time::Duration::from_millis(200));
    let _ = repo
        .worktree_is_clean_cached_with_options(&tree, &native_options)
        .unwrap();
    group.bench_with_input(
        BenchmarkId::new("native", file_count),
        &file_count,
        |b, _| {
            b.iter(|| {
                let clean = repo
                    .worktree_is_clean_cached_with_options(&tree, &native_options)
                    .unwrap();
                black_box(clean);
            });
        },
    );
    group.finish();
}

fn bench_worktree_clean_native_profile(c: &mut Criterion) {
    let mut group = c.benchmark_group("worktree_is_clean_native_profile");
    let file_count = 1_000usize;
    let (_temp, repo) = setup_repo_with_files(file_count);
    let state = repo.snapshot(Some("base".to_string()), None).unwrap();
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();
    let native_options = WorktreeStatusOptions {
        fsmonitor: FsMonitorSettings {
            mode: FsMonitorMode::Native,
        },
    };
    let root = repo.root().to_path_buf();
    std::thread::spawn(move || {
        let _ = run_local_monitor_helper(&root);
    });
    std::thread::sleep(std::time::Duration::from_millis(200));
    let _ = repo
        .compare_worktree_cached_profiled_with_options(&tree, &native_options)
        .unwrap();

    group.bench_with_input(
        BenchmarkId::from_parameter(file_count),
        &file_count,
        |b, _| {
            b.iter(|| {
                let (status, profile) = repo
                    .compare_worktree_cached_profiled_with_options(&tree, &native_options)
                    .unwrap();
                black_box((
                    status.is_clean(),
                    profile.monitor_prepare_ms,
                    profile.compare_ms,
                    profile.monitor_persist_ms,
                ));
            });
        },
    );
    group.finish();
}

fn bench_goto_same_tree(c: &mut Criterion) {
    let mut group = c.benchmark_group("goto_same_tree");
    for &file_count in &[100usize, 1_000] {
        let (_temp, repo) = setup_repo_with_files(file_count);
        let state = repo.snapshot(Some("base".to_string()), None).unwrap();

        group.bench_with_input(
            BenchmarkId::from_parameter(file_count),
            &file_count,
            |b, _| {
                b.iter(|| {
                    repo.goto(&state.change_id).unwrap();
                    black_box(state.change_id);
                });
            },
        );
    }
    group.finish();
}

fn bench_goto_small_delta(c: &mut Criterion) {
    let mut group = c.benchmark_group("goto_small_delta");
    let file_count = 1_000usize;
    let (_temp, repo) = setup_repo_with_files(file_count);
    let base = repo.snapshot(Some("base".to_string()), None).unwrap();

    std::fs::write(
        repo.root().join("tracked/dir-00/file-00000.txt"),
        "small delta\n",
    )
    .unwrap();
    let delta = repo.snapshot(Some("delta".to_string()), None).unwrap();

    repo.goto(&base.change_id).unwrap();
    let mut target = delta.change_id;
    group.bench_with_input(
        BenchmarkId::from_parameter(file_count),
        &file_count,
        |b, _| {
            b.iter(|| {
                repo.goto(&target).unwrap();
                target = if target == delta.change_id {
                    base.change_id
                } else {
                    delta.change_id
                };
                black_box(target);
            });
        },
    );
    group.finish();
}

fn bench_goto_large_delta(c: &mut Criterion) {
    let mut group = c.benchmark_group("goto_large_delta");
    for &file_count in &[1_000usize] {
        let (_temp, repo) = setup_repo_with_files(file_count);
        let base = repo.snapshot(Some("base".to_string()), None).unwrap();

        for i in 0..file_count {
            std::fs::write(
                repo.root()
                    .join(format!("tracked/dir-{:02}/file-{i:05}.txt", i % 20)),
                format!("rewritten file {i}\n{}\n", "y".repeat(64)),
            )
            .unwrap();
        }
        let delta = repo.snapshot(Some("delta".to_string()), None).unwrap();

        repo.goto(&base.change_id).unwrap();
        let mut target = delta.change_id;
        group.bench_with_input(
            BenchmarkId::from_parameter(file_count),
            &file_count,
            |b, _| {
                b.iter(|| {
                    repo.goto(&target).unwrap();
                    target = if target == delta.change_id {
                        base.change_id
                    } else {
                        delta.change_id
                    };
                    black_box(target);
                });
            },
        );
    }
    group.finish();
}

fn bench_goto_same_tree_then_status(c: &mut Criterion) {
    let mut group = c.benchmark_group("goto_same_tree_then_status");
    let file_count = 1_000usize;
    let (_temp, repo) = setup_repo_with_files(file_count);
    let state = repo.snapshot(Some("base".to_string()), None).unwrap();
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();

    group.bench_with_input(
        BenchmarkId::from_parameter(file_count),
        &file_count,
        |b, _| {
            b.iter(|| {
                repo.goto(&state.change_id).unwrap();
                let clean = repo.worktree_is_clean_cached(&tree).unwrap();
                black_box((state.change_id, clean));
            });
        },
    );
    group.finish();
}

fn bench_goto_small_delta_then_status(c: &mut Criterion) {
    let mut group = c.benchmark_group("goto_small_delta_then_status");
    let file_count = 1_000usize;
    let (_temp, repo) = setup_repo_with_files(file_count);
    let base = repo.snapshot(Some("base".to_string()), None).unwrap();
    std::fs::write(
        repo.root().join("tracked/dir-00/file-00000.txt"),
        "small delta\n",
    )
    .unwrap();
    let delta = repo.snapshot(Some("delta".to_string()), None).unwrap();
    let base_tree = repo.store().get_tree(&base.tree).unwrap().unwrap();
    let delta_tree = repo.store().get_tree(&delta.tree).unwrap().unwrap();

    repo.goto(&base.change_id).unwrap();
    let mut target = delta.change_id;
    group.bench_with_input(
        BenchmarkId::from_parameter(file_count),
        &file_count,
        |b, _| {
            b.iter(|| {
                repo.goto(&target).unwrap();
                let clean = if target == delta.change_id {
                    repo.worktree_is_clean_cached(&delta_tree).unwrap()
                } else {
                    repo.worktree_is_clean_cached(&base_tree).unwrap()
                };
                target = if target == delta.change_id {
                    base.change_id
                } else {
                    delta.change_id
                };
                black_box((target, clean));
            });
        },
    );
    group.finish();
}

fn bench_full_rematerialize_then_status(c: &mut Criterion) {
    let mut group = c.benchmark_group("full_rematerialize_then_status");
    let file_count = 1_000usize;
    let (_temp, repo) = setup_repo_with_files(file_count);
    let base = repo.snapshot(Some("base".to_string()), None).unwrap();

    std::fs::write(
        repo.root().join("tracked/dir-00/file-00000.txt"),
        "delta version\n",
    )
    .unwrap();
    let delta = repo.snapshot(Some("delta".to_string()), None).unwrap();
    let base_tree = repo.store().get_tree(&base.tree).unwrap().unwrap();
    let delta_tree = repo.store().get_tree(&delta.tree).unwrap().unwrap();

    repo.goto(&base.change_id).unwrap();

    group.bench_with_input(
        BenchmarkId::from_parameter(file_count),
        &file_count,
        |b, _| {
            b.iter(|| {
                std::fs::write(
                    repo.root().join("tracked/dir-03/file-00003.txt"),
                    "dirty worktree forces fallback\n",
                )
                .unwrap();
                repo.goto(&delta.change_id).unwrap();
                let delta_clean = repo.worktree_is_clean_cached(&delta_tree).unwrap();
                repo.goto(&base.change_id).unwrap();
                let base_clean = repo.worktree_is_clean_cached(&base_tree).unwrap();
                black_box((delta_clean, base_clean));
            });
        },
    );
    group.finish();
}

fn setup_divergent_history() -> (
    TempDir,
    Repository,
    objects::object::ChangeId,
    objects::object::ChangeId,
    objects::object::ChangeId,
) {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    std::fs::write(temp.path().join("shared.txt"), "base\n").unwrap();
    let base = repo.snapshot(Some("base".to_string()), None).unwrap();

    repo.refs()
        .set_thread(&ThreadName::new("topic"), &base.change_id)
        .unwrap();

    std::fs::write(temp.path().join("main-only.txt"), "main\n").unwrap();
    let main_tip = repo.snapshot(Some("main tip".to_string()), None).unwrap();

    repo.refs()
        .write_head(&Head::Attached {
            thread: ThreadName::new("topic"),
        })
        .unwrap();
    repo.goto(&base.change_id).unwrap();
    std::fs::write(temp.path().join("topic-only.txt"), "topic\n").unwrap();
    let topic_tip = repo.snapshot(Some("topic tip".to_string()), None).unwrap();

    repo.refs()
        .write_head(&Head::Attached {
            thread: ThreadName::new("main"),
        })
        .unwrap();
    repo.goto(&main_tip.change_id).unwrap();

    (
        temp,
        repo,
        base.change_id,
        main_tip.change_id,
        topic_tip.change_id,
    )
}

fn bench_find_merge_base(c: &mut Criterion) {
    let (_temp, repo, _base, main_tip, topic_tip) = setup_divergent_history();
    c.bench_function("find_merge_base", |b| {
        b.iter(|| {
            let merge_base = find_merge_base_for_bench(&repo, &main_tip, &topic_tip).unwrap();
            black_box(merge_base);
        });
    });
}

fn bench_three_way_merge(c: &mut Criterion) {
    let (_temp, repo, base_id, main_tip, topic_tip) = setup_divergent_history();
    let base_tree = repo.get_tree_for_state(&base_id).unwrap().unwrap();
    let main_tree = repo.get_tree_for_state(&main_tip).unwrap().unwrap();
    let topic_tree = repo.get_tree_for_state(&topic_tip).unwrap().unwrap();

    c.bench_function("three_way_merge", |b| {
        b.iter(|| {
            let result =
                three_way_merge_for_bench(&repo, &base_tree, &main_tree, &topic_tree).unwrap();
            black_box(result.1 + result.2 + result.3);
        });
    });
}

fn make_bench_tree(store: &InMemoryStore, files: &[(&str, &[u8])]) -> Tree {
    let entries: Vec<TreeEntry> = files
        .iter()
        .map(|(path, content)| {
            let hash = store.put_blob(&Blob::new(content.to_vec())).unwrap();
            TreeEntry::file((*path).to_string(), hash, false).unwrap()
        })
        .collect();
    Tree::from_entries(entries)
}

fn bench_detect_renames(c: &mut Criterion) {
    let mut group = c.benchmark_group("detect_renames");
    for &file_count in &[100usize, 500] {
        let store = InMemoryStore::new();
        let mut base_files = Vec::with_capacity(file_count);
        let mut branch_files = Vec::with_capacity(file_count);

        for i in 0..file_count {
            let content = format!(
                "fn item_{i}() {{\n    let value = {i};\n    println!(\"{{}}\", value);\n}}\n{}",
                "x".repeat(96)
            )
            .into_bytes();
            base_files.push((format!("original_{i}.rs"), content.clone()));
            branch_files.push((format!("renamed_{i}.rs"), content));
        }

        let base_refs: Vec<(&str, &[u8])> = base_files
            .iter()
            .map(|(path, content)| (path.as_str(), content.as_slice()))
            .collect();
        let branch_refs: Vec<(&str, &[u8])> = branch_files
            .iter()
            .map(|(path, content)| (path.as_str(), content.as_slice()))
            .collect();
        let base_tree = make_bench_tree(&store, &base_refs);
        let branch_tree = make_bench_tree(&store, &branch_refs);

        group.bench_with_input(
            BenchmarkId::from_parameter(file_count),
            &file_count,
            |b, _| {
                b.iter(|| {
                    let result =
                        detect_renames_for_bench(&store, &base_tree, &branch_tree).unwrap();
                    black_box(result);
                });
            },
        );
    }
    group.finish();
}

fn setup_semantic_history() -> (
    TempDir,
    Repository,
    objects::object::ContentHash,
    objects::object::ContentHash,
) {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    std::fs::write(
        temp.path().join("lib.rs"),
        "fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    let base = repo.snapshot(Some("base".to_string()), None).unwrap();

    std::fs::write(
        temp.path().join("lib.rs"),
        "fn add(a:i32,b:i32)->i32 {\n    a + b\n}\n",
    )
    .unwrap();
    let formatted = repo.snapshot(Some("formatted".to_string()), None).unwrap();

    (temp, repo, base.tree, formatted.tree)
}

fn bench_semantic_diff(c: &mut Criterion) {
    let (_temp, repo, base_tree, formatted_tree) = setup_semantic_history();
    let options = SemanticDiffOptions {
        similarity_method: SimilarityMethod::Ast,
        ..Default::default()
    };

    c.bench_function("semantic_diff", |b| {
        b.iter(|| {
            let result =
                semantic_diff(repo.store(), &base_tree, &formatted_tree, &options).unwrap();
            black_box(result.changes.len());
        });
    });
}

fn bench_semantic_check_only(c: &mut Criterion) {
    let (_temp, repo, base_tree, formatted_tree) = setup_semantic_history();
    let options = SemanticDiffOptions {
        similarity_method: SimilarityMethod::Ast,
        ..Default::default()
    };

    c.bench_function("semantic_check_only", |b| {
        b.iter(|| {
            let result =
                semantic_check_only(repo.store(), &base_tree, &formatted_tree, &options).unwrap();
            black_box(result.status);
        });
    });
}

fn bench_semantic_summary(c: &mut Criterion) {
    let (_temp, repo, base_tree, formatted_tree) = setup_semantic_history();
    let options = SemanticDiffOptions {
        similarity_method: SimilarityMethod::Ast,
        ..Default::default()
    };

    c.bench_function("semantic_diff_summary", |b| {
        b.iter(|| {
            let result =
                semantic_diff_summary(repo.store(), &base_tree, &formatted_tree, &options).unwrap();
            black_box(result.file_renames.len());
        });
    });
}

fn bench_semantic_parse_cache_warm(c: &mut Criterion) {
    let cache = SemanticParseCache::shared();
    cache.clear();
    let (_temp, repo, base_tree, formatted_tree) = setup_semantic_history();
    let options = SemanticDiffOptions {
        similarity_method: SimilarityMethod::Ast,
        ..Default::default()
    };

    let _ = semantic_diff(repo.store(), &base_tree, &formatted_tree, &options).unwrap();

    c.bench_function("semantic_parse_cache_warm", |b| {
        b.iter(|| {
            let before = cache.stats();
            let result =
                semantic_diff(repo.store(), &base_tree, &formatted_tree, &options).unwrap();
            let after = cache.stats();
            black_box((result.changes.len(), after.hits.saturating_sub(before.hits)));
        });
    });
}

fn bench_semantic_parse_cache_cold(c: &mut Criterion) {
    let cache = SemanticParseCache::shared();
    let (_temp, repo, base_tree, formatted_tree) = setup_semantic_history();
    let options = SemanticDiffOptions {
        similarity_method: SimilarityMethod::Ast,
        ..Default::default()
    };

    c.bench_function("semantic_parse_cache_cold", |b| {
        b.iter(|| {
            cache.clear();
            let before = cache.stats();
            let result =
                semantic_diff(repo.store(), &base_tree, &formatted_tree, &options).unwrap();
            let after = cache.stats();
            black_box((
                result.changes.len(),
                after.misses.saturating_sub(before.misses),
            ));
        });
    });
}

criterion_group!(
    local_ops,
    bench_build_tree,
    bench_build_tree_cold_shapes,
    bench_snapshot_profile_flat_repo,
    bench_compare_worktree,
    bench_compare_worktree_many_added_files,
    bench_compare_worktree_untracked_dirs_with_tracked_changes,
    bench_compare_worktree_untracked_dirs_second_run,
    bench_refs_list_threads_summary,
    bench_refs_list_markers_summary,
    bench_refs_list_remote_threads_summary,
    bench_refs_update_thread_rebuild_summary,
    bench_refs_update_marker_rebuild_summary,
    bench_refs_update_remote_thread_rebuild_summary,
    bench_worktree_is_clean_untracked_dirs_second_run,
    bench_worktree_clean,
    bench_worktree_clean_modes,
    bench_worktree_clean_native_profile,
    bench_goto_same_tree,
    bench_goto_small_delta,
    bench_goto_large_delta,
    bench_goto_same_tree_then_status,
    bench_goto_small_delta_then_status,
    bench_full_rematerialize_then_status,
    bench_find_merge_base,
    bench_three_way_merge,
    bench_detect_renames,
    bench_semantic_check_only,
    bench_semantic_summary,
    bench_semantic_diff,
    bench_semantic_parse_cache_warm,
    bench_semantic_parse_cache_cold
);

#[cfg(unix)]
criterion_group!(unix_status_ops, bench_compare_worktree_symlink_heavy);

#[cfg(unix)]
criterion_main!(local_ops, unix_status_ops);

#[cfg(not(unix))]
criterion_main!(local_ops);
