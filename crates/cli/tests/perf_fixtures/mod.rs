// SPDX-License-Identifier: Apache-2.0
//! Test-only performance fixture scaffold shared by CLI perf tests.

use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

use objects::{object::ThreadName, store::ObjectStore};
use repo::Repository;
use tempfile::TempDir;

const SMOKE_SMALL_FILE_COUNT: usize = 24;
const SMOKE_LARGE_BLOB_COUNT: usize = 3;
const SMOKE_LARGE_BLOB_BYTES: usize = 2 * 1024 * 1024;
const SMOKE_EXTRA_REF_COUNT: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FixtureShape {
    ManySmallFiles,
    MultiMbBlobs,
    DeepHistory,
    ManyRefs,
    RenameMove,
    GitOverlayImportExport,
    HostedNativeSync,
    SemanticMerge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FixtureGate {
    Default,
    Ignored,
    Release,
    Env(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FixtureDefinition {
    pub id: &'static str,
    pub shape: FixtureShape,
    pub smoke_covered: bool,
    pub gate: FixtureGate,
}

#[derive(Clone, Debug)]
pub struct FixtureMetrics {
    pub build_wall_time: Duration,
    pub peak_rss_bytes: Option<u64>,
    pub object_count: usize,
    pub pack_count: usize,
    pub logical_bytes_read: u64,
    pub logical_bytes_written: u64,
    pub cold_status_wall_time: Duration,
    pub warm_status_wall_time: Duration,
    pub thread_ref_count: usize,
}

#[derive(Debug)]
pub struct BuiltFixture {
    temp: TempDir,
    metrics: FixtureMetrics,
    covered_shapes: Vec<FixtureShape>,
}

impl BuiltFixture {
    pub fn root(&self) -> &Path {
        self.temp.path()
    }

    pub fn metrics(&self) -> &FixtureMetrics {
        &self.metrics
    }

    pub fn covers(&self, shape: FixtureShape) -> bool {
        self.covered_shapes.contains(&shape)
    }
}

pub fn required_shapes() -> &'static [FixtureShape] {
    &[
        FixtureShape::ManySmallFiles,
        FixtureShape::MultiMbBlobs,
        FixtureShape::DeepHistory,
        FixtureShape::ManyRefs,
        FixtureShape::RenameMove,
        FixtureShape::GitOverlayImportExport,
        FixtureShape::HostedNativeSync,
        FixtureShape::SemanticMerge,
    ]
}

pub fn fixture_catalog() -> &'static [FixtureDefinition] {
    &[
        FixtureDefinition {
            id: "many-small-files",
            shape: FixtureShape::ManySmallFiles,
            smoke_covered: true,
            gate: FixtureGate::Default,
        },
        FixtureDefinition {
            id: "multi-mb-blobs",
            shape: FixtureShape::MultiMbBlobs,
            smoke_covered: true,
            gate: FixtureGate::Default,
        },
        FixtureDefinition {
            id: "deep-history",
            shape: FixtureShape::DeepHistory,
            smoke_covered: true,
            gate: FixtureGate::Default,
        },
        FixtureDefinition {
            id: "many-refs",
            shape: FixtureShape::ManyRefs,
            smoke_covered: true,
            gate: FixtureGate::Default,
        },
        FixtureDefinition {
            id: "rename-move",
            shape: FixtureShape::RenameMove,
            smoke_covered: true,
            gate: FixtureGate::Default,
        },
        FixtureDefinition {
            id: "git-overlay-import-export",
            shape: FixtureShape::GitOverlayImportExport,
            smoke_covered: false,
            gate: FixtureGate::Ignored,
        },
        FixtureDefinition {
            id: "hosted-native-sync",
            shape: FixtureShape::HostedNativeSync,
            smoke_covered: false,
            gate: FixtureGate::Env("HEDDLE_PERF_HOSTED_TARGET"),
        },
        FixtureDefinition {
            id: "semantic-merge",
            shape: FixtureShape::SemanticMerge,
            smoke_covered: false,
            gate: FixtureGate::Release,
        },
    ]
}

pub fn build_default_smoke_fixture() -> BuiltFixture {
    let started = Instant::now();
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let mut logical_bytes_written = 0_u64;

    logical_bytes_written += write_many_small_files(temp.path(), SMOKE_SMALL_FILE_COUNT);
    logical_bytes_written +=
        write_large_blobs(temp.path(), SMOKE_LARGE_BLOB_COUNT, SMOKE_LARGE_BLOB_BYTES);
    let base = repo
        .snapshot(Some("perf smoke base".to_string()), None)
        .unwrap();

    for index in 0..SMOKE_EXTRA_REF_COUNT {
        repo.refs()
            .set_thread(
                &ThreadName::new(format!("perf/ref-{index:02}")),
                &base.change_id,
            )
            .unwrap();
    }

    for index in 0..3 {
        let path = temp.path().join("history").join("counter.txt");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let content = format!("history step {index}\n");
        fs::write(&path, content.as_bytes()).unwrap();
        logical_bytes_written += content.len() as u64;
        repo.snapshot(Some(format!("perf smoke history {index}")), None)
            .unwrap();
    }

    let moved_dir = temp.path().join("src").join("renamed");
    fs::create_dir_all(&moved_dir).unwrap();
    fs::rename(
        temp.path().join("src/shard-00/file-000.txt"),
        moved_dir.join("file-000.rs"),
    )
    .unwrap();
    let tip = repo
        .snapshot(Some("perf smoke rename move".to_string()), None)
        .unwrap();

    let tree = repo.store().get_tree(&tip.tree).unwrap().unwrap();
    repo.store().clear_recent_caches();
    let cold_start = Instant::now();
    let cold_status = repo.compare_worktree_cached(&tree).unwrap();
    let cold_status_wall_time = cold_start.elapsed();
    assert!(cold_status.is_clean());

    let warm_start = Instant::now();
    let warm_status = repo.compare_worktree_cached(&tree).unwrap();
    let warm_status_wall_time = warm_start.elapsed();
    assert!(warm_status.is_clean());

    let logical_bytes_read = read_large_blob_fixture_bytes(temp.path(), SMOKE_LARGE_BLOB_COUNT);
    let metrics = FixtureMetrics {
        build_wall_time: started.elapsed(),
        peak_rss_bytes: peak_rss_bytes(),
        object_count: repo.store().list_blobs().unwrap().len()
            + repo.store().list_trees().unwrap().len()
            + repo.store().list_states().unwrap().len()
            + repo.store().list_actions().unwrap().len(),
        pack_count: pack_count_in(repo.heddle_dir().join("packs").as_path()),
        logical_bytes_read,
        logical_bytes_written,
        cold_status_wall_time,
        warm_status_wall_time,
        thread_ref_count: repo.refs().list_threads().unwrap().len(),
    };

    BuiltFixture {
        temp,
        metrics,
        covered_shapes: fixture_catalog()
            .iter()
            .filter(|definition| definition.smoke_covered)
            .map(|definition| definition.shape)
            .collect(),
    }
}

fn write_many_small_files(root: &Path, count: usize) -> u64 {
    let mut bytes = 0_u64;
    for index in 0..count {
        let dir = root.join(format!("src/shard-{:02}", index % 4));
        fs::create_dir_all(&dir).unwrap();
        let content = format!("fn fixture_{index:03}() -> usize {{ {index} }}\n");
        fs::write(dir.join(format!("file-{index:03}.txt")), content.as_bytes()).unwrap();
        bytes += content.len() as u64;
    }
    bytes
}

fn write_large_blobs(root: &Path, count: usize, size: usize) -> u64 {
    let dir = root.join("assets");
    fs::create_dir_all(&dir).unwrap();
    let mut total = 0_u64;
    for index in 0..count {
        let bytes = deterministic_bytes(size, index);
        fs::write(dir.join(format!("large-blob-{index:02}.bin")), &bytes).unwrap();
        total += bytes.len() as u64;
    }
    total
}

fn read_large_blob_fixture_bytes(root: &Path, count: usize) -> u64 {
    (0..count)
        .map(|index| {
            fs::read(root.join(format!("assets/large-blob-{index:02}.bin")))
                .map(|bytes| bytes.len() as u64)
                .unwrap_or(0)
        })
        .sum()
}

fn deterministic_bytes(size: usize, seed: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(size);
    for index in 0..size {
        bytes.push(((index * 131 + seed * 17 + (index >> 7) * 17) & 0xff) as u8);
    }
    bytes
}

fn pack_count_in(packs_dir: &Path) -> usize {
    fs::read_dir(packs_dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry.path().extension().and_then(|ext| ext.to_str()) == Some("pack")
                })
                .count()
        })
        .unwrap_or(0)
}

#[cfg(target_os = "macos")]
fn peak_rss_bytes() -> Option<u64> {
    peak_rss_raw().map(|rss| rss as u64)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn peak_rss_bytes() -> Option<u64> {
    peak_rss_raw().map(|rss| (rss as u64).saturating_mul(1024))
}

#[cfg(not(unix))]
fn peak_rss_bytes() -> Option<u64> {
    None
}

#[cfg(unix)]
fn peak_rss_raw() -> Option<libc::c_long> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if rc == 0 {
        Some(unsafe { usage.assume_init().ru_maxrss })
    } else {
        None
    }
}
