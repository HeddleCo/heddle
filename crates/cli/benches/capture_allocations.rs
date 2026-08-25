// SPDX-License-Identifier: Apache-2.0
//! Allocation evidence for the one-path native capture hot path.
//!
//! This is deliberately separate from the Criterion latency suite: installing
//! an instrumented global allocator in `local_ops` would add accounting work to
//! every timed allocation and contaminate the release performance contract.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    env,
    hint::black_box,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use objects::{object::Tree, store::ObjectStore};
use repo::{
    FsMonitorMode, FsMonitorSettings, Repository, WorktreeStatusOptions, run_local_monitor_helper,
};
use tempfile::TempDir;

struct CountingAllocator;

thread_local! {
    /// Count only allocations made by the capture thread. The native monitor
    /// helper remains live in the background and would otherwise add noise.
    static COUNT_THIS_THREAD: Cell<bool> = const { Cell::new(false) };
}

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNT_THIS_THREAD.try_with(Cell::get).unwrap_or(false) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        // SAFETY: the unchanged allocation request is delegated to `System`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` and `layout` came from the delegated system allocation.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNT_THIS_THREAD.try_with(Cell::get).unwrap_or(false) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        }
        // SAFETY: the unchanged reallocation request is delegated to `System`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

fn measure_allocations<T>(operation: impl FnOnce() -> T) -> (T, u64, u64) {
    ALLOCATIONS.store(0, Ordering::Relaxed);
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    COUNT_THIS_THREAD.with(|enabled| enabled.set(true));
    let result = operation();
    COUNT_THIS_THREAD.with(|enabled| enabled.set(false));
    (
        result,
        ALLOCATIONS.load(Ordering::Relaxed),
        ALLOCATED_BYTES.load(Ordering::Relaxed),
    )
}

fn write_files(root: &Path, count: usize) {
    for index in 0..count {
        let directory = root.join(format!("tracked/dir-{:02}", index % 20));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join(format!("file-{index:05}.txt")),
            format!("tracked file {index}\n{}\n", "x".repeat(64)),
        )
        .unwrap();
    }
}

fn setup_warm_native_repo(count: usize) -> (TempDir, Repository, Tree) {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path())
        .unwrap()
        .without_fsmonitor();
    write_files(temp.path(), count);
    let state = repo.snapshot(Some("base".to_string()), None).unwrap();
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();
    let mut config = repo.config().clone();
    config.worktree.fsmonitor.mode = FsMonitorMode::Native;
    config.save(&repo.heddle_dir().join("config.toml")).unwrap();
    drop(repo);

    let monitor_root = temp.path().to_path_buf();
    std::thread::spawn(move || {
        let _ = run_local_monitor_helper(&monitor_root);
    });
    let repo = Repository::open(temp.path()).unwrap();
    let options = WorktreeStatusOptions {
        fsmonitor: FsMonitorSettings {
            mode: FsMonitorMode::Native,
        },
    };
    for _ in 0..100 {
        let (_, profile) = repo
            .compare_worktree_cached_profiled_with_options(&tree, &options)
            .unwrap();
        if profile.scan_mode == "changed_paths" {
            return (temp, repo, tree);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("native monitor did not become usable for allocation fixture");
}

fn path_counts() -> Vec<usize> {
    env::var("HEDDLE_BENCH_CAPTURE_ALLOCATION_COUNTS")
        .ok()
        .map(|value| {
            value
                .split(',')
                .map(|count| count.trim().parse().expect("path count must be an integer"))
                .collect()
        })
        .unwrap_or_else(|| vec![1_000, 100_000])
}

fn main() {
    for path_count in path_counts() {
        let (_temp, repo, baseline_tree) = setup_warm_native_repo(path_count);
        let dirty_path = repo.root().join("tracked/dir-00/file-00000.txt");
        std::fs::write(&dirty_path, "allocation probe\n").unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let attribution = repo.get_attribution().unwrap();

        let (execution, allocations, allocated_bytes) = measure_allocations(|| {
            repo.snapshot_with_attribution_profiled(
                Some("allocation probe".to_string()),
                None,
                attribution,
            )
            .unwrap()
        });
        assert_ne!(
            execution.tree.hash(),
            baseline_tree.hash(),
            "native monitor did not observe the allocation probe edit"
        );
        black_box(execution.state.state_id);
        println!(
            "capture_one_path paths={path_count} allocations={allocations} allocated_bytes={allocated_bytes}"
        );
    }
}
