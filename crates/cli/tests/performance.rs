//! Performance and large file handling tests.
//!
//! Tests for verifying system behavior with large repositories and files.

use objects::store::ObjectStore;
use std::{
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

use objects::{
    object::{Blob, ChangeId, ContentHash},
    store::{CompressionConfig, PackBuilder, PackObjectId, pack::ObjectType as PackObjectType},
};
use proto::{ObjectData, ObjectId, ObjectType};
use repo::Repository;
use tempfile::TempDir;

#[derive(Debug)]
struct SnapshotProfile {
    file_count: usize,
    tree_walk_ms: u128,
    blob_prep_ms: u128,
    blob_write_ms: u128,
    tree_write_ms: u128,
    state_ref_oplog_ms: u128,
    snapshot_total: Duration,
    git_add: Option<Duration>,
    git_commit: Option<Duration>,
}

fn write_snapshot_bench_files(root: &Path, file_count: usize) {
    for i in 0..file_count {
        std::fs::write(
            root.join(format!("file_{i:05}.txt")),
            format!("content {i}\n{}\n", "x".repeat(48)),
        )
        .unwrap();
    }
}

fn try_run_git(dir: &Path, args: &[&str]) -> Option<()> {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .ok()?;
    status.success().then_some(())
}

fn try_git_snapshot_baseline(file_count: usize) -> Option<(Duration, Duration)> {
    let version = Command::new("git").arg("--version").status().ok()?;
    if !version.success() {
        return None;
    }

    let temp = TempDir::new().ok()?;
    try_run_git(temp.path(), &["init", "-q"])?;
    try_run_git(temp.path(), &["config", "user.name", "Heddle Bench"])?;
    try_run_git(
        temp.path(),
        &["config", "user.email", "heddle-bench@example.com"],
    )?;
    write_snapshot_bench_files(temp.path(), file_count);

    let add_start = Instant::now();
    try_run_git(temp.path(), &["add", "-A"])?;
    let add_elapsed = add_start.elapsed();

    let commit_start = Instant::now();
    try_run_git(temp.path(), &["commit", "-qm", "benchmark"])?;
    let commit_elapsed = commit_start.elapsed();

    Some((add_elapsed, commit_elapsed))
}

fn measure_snapshot_profile(file_count: usize) -> SnapshotProfile {
    let snapshot_temp = TempDir::new().unwrap();
    let snapshot_repo = Repository::init_default(snapshot_temp.path()).unwrap();
    write_snapshot_bench_files(snapshot_temp.path(), file_count);
    let attribution = snapshot_repo.get_attribution().unwrap();

    let snapshot_start = Instant::now();
    let result = snapshot_repo.snapshot_with_attribution_profiled(
        Some("Many files".to_string()),
        None,
        attribution,
    );
    let snapshot_total = snapshot_start.elapsed();

    let execution = result.expect("repository snapshot should succeed");
    let (git_add, git_commit) = try_git_snapshot_baseline(file_count)
        .map(|(add, commit)| (Some(add), Some(commit)))
        .unwrap_or((None, None));

    SnapshotProfile {
        file_count,
        tree_walk_ms: execution.profile.tree_walk_ms,
        blob_prep_ms: execution.profile.blob_prep_ms,
        blob_write_ms: execution.profile.blob_write_ms,
        tree_write_ms: execution.profile.tree_write_ms,
        state_ref_oplog_ms: execution.profile.state_ref_oplog_ms,
        snapshot_total,
        git_add,
        git_commit,
    }
}

fn print_snapshot_profile(label: &str, profile: &SnapshotProfile) {
    println!(
        "{label}: files={} tree_walk_ms={} blob_prep_ms={} blob_write_ms={} tree_write_ms={} state_ref_oplog_ms={} snapshot_total={:?}",
        profile.file_count,
        profile.tree_walk_ms,
        profile.blob_prep_ms,
        profile.blob_write_ms,
        profile.tree_write_ms,
        profile.state_ref_oplog_ms,
        profile.snapshot_total,
    );

    match (profile.git_add, profile.git_commit) {
        (Some(add), Some(commit)) => println!(
            "{label}: git baseline add={:?} commit={:?} total={:?}",
            add,
            commit,
            add + commit
        ),
        _ => println!("{label}: git baseline unavailable; skipping parity output"),
    }
}

/// Test snapshot performance with many small files.
///
/// The fast path packs every new blob into a single packfile (one
/// `.pack` + one `.idx`, two fsyncs total) rather than writing 1000
/// loose blobs with a fsync per file. Debug-mode measurements on
/// macOS APFS land around 150 ms; the 1-second budget is the
/// "something regressed" alarm. Tighten only after benchmarking on
/// the slowest CI runner — laptop SSDs are not the worst case.
#[test]
fn test_snapshot_many_small_files() {
    let profile = measure_snapshot_profile(1_000);
    print_snapshot_profile("repository snapshot 1000 files", &profile);

    assert!(
        profile.snapshot_total < Duration::from_secs(1),
        "Snapshot of 1000 files should take less than 1 second, took {:?}",
        profile.snapshot_total
    );
}

/// Test handling of large individual files.
#[test]
fn test_large_file_handling() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create a 10MB file
    let large_content = vec![b'x'; 10 * 1024 * 1024];
    std::fs::write(temp.path().join("large.bin"), &large_content).unwrap();

    let start = Instant::now();
    let result = repo.snapshot(Some("Large file".to_string()), None);
    let elapsed = start.elapsed();

    assert!(result.is_ok());
    assert!(
        elapsed.as_secs() < 60,
        "Snapshot with 10MB file should take less than 60 seconds, took {:?}",
        elapsed
    );

    // Verify blob was stored
    let state = result.unwrap();
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();
    assert!(tree.get("large.bin").is_some());
}

/// Test deep directory structure performance.
#[test]
fn test_deep_directory_structure() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create deeply nested directories (50 levels)
    let mut path = temp.path().to_path_buf();
    for i in 0..50 {
        path = path.join(format!("level{}", i));
        std::fs::create_dir(&path).unwrap();
    }

    // Add file at deepest level
    std::fs::write(path.join("deep_file.txt"), "deep content").unwrap();

    let start = Instant::now();
    let result = repo.snapshot(Some("Deep tree".to_string()), None);
    let elapsed = start.elapsed();

    assert!(result.is_ok());
    assert!(
        elapsed.as_secs() < 30,
        "Snapshot of deep tree should take less than 30 seconds, took {:?}",
        elapsed
    );
}

/// Test wide directory structure (many directories at same level).
#[test]
fn test_wide_directory_structure() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create 100 directories at the same level, each with a file
    for i in 0..100 {
        let dir = temp.path().join(format!("dir{}", i));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("file.txt"), format!("content {}", i)).unwrap();
    }

    let start = Instant::now();
    let result = repo.snapshot(Some("Wide tree".to_string()), None);
    let elapsed = start.elapsed();

    assert!(result.is_ok());
    assert!(
        elapsed.as_secs() < 30,
        "Snapshot of wide tree should take less than 30 seconds, took {:?}",
        elapsed
    );
}

/// Test blob deduplication performance with duplicate content.
#[test]
fn test_deduplication_performance() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create same large content in multiple files
    let content = vec![b'd'; 1024 * 1024]; // 1MB of 'd'

    for i in 0..100 {
        std::fs::write(temp.path().join(format!("dup_{}.bin", i)), &content).unwrap();
    }

    let start = Instant::now();
    let result = repo.snapshot(Some("Duplicates".to_string()), None);
    let elapsed = start.elapsed();

    assert!(result.is_ok());

    // With deduplication, this should be relatively fast
    // Even though we have 100MB of logical content, it's only 1MB physical
    println!("Snapshot of 100 duplicate 1MB files took {:?}", elapsed);
}

/// Test log performance with long history.
#[test]
fn test_log_performance_many_states() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create 100 states in history
    for i in 0..100 {
        std::fs::write(
            temp.path().join(format!("v{}.txt", i)),
            format!("version {}", i),
        )
        .unwrap();
        repo.snapshot(Some(format!("State {}", i)), None).unwrap();
    }

    // Time listing all states
    let start = Instant::now();
    let states = repo.store().list_states().unwrap();
    let elapsed = start.elapsed();

    // 100 user snapshots + 1 bootstrap state from init_default
    assert_eq!(states.len(), 101);
    assert!(
        elapsed.as_millis() < 1000,
        "Listing 101 states should take less than 1 second, took {:?}",
        elapsed
    );
}

/// Test goto performance switching between states.
#[test]
fn test_goto_performance() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create several states
    let mut state_ids = Vec::new();
    for i in 0..10 {
        std::fs::write(temp.path().join("data.txt"), format!("data {}", i)).unwrap();
        let state = repo.snapshot(Some(format!("State {}", i)), None).unwrap();
        state_ids.push(state.change_id);
    }

    // Time switching between states
    let start = Instant::now();
    for state_id in &state_ids {
        repo.goto(state_id).unwrap();
    }
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_secs() < 30,
        "10 goto operations should take less than 30 seconds, took {:?}",
        elapsed
    );
}

/// Test diff performance with large trees.
#[test]
fn test_diff_performance_large_trees() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create base state with many files
    for i in 0..500 {
        std::fs::write(
            temp.path().join(format!("file_{}.txt", i)),
            format!("content {}", i),
        )
        .unwrap();
    }
    let base = repo.snapshot(Some("Base".to_string()), None).unwrap();

    // Modify half the files
    for i in 0..250 {
        std::fs::write(
            temp.path().join(format!("file_{}.txt", i)),
            format!("modified {}", i),
        )
        .unwrap();
    }
    let modified = repo.snapshot(Some("Modified".to_string()), None).unwrap();

    // Time the diff
    let start = Instant::now();
    let changes = repo.diff_trees(&base.tree, &modified.tree).unwrap();
    let elapsed = start.elapsed();

    assert_eq!(changes.len(), 250);
    assert!(
        elapsed.as_secs() < 10,
        "Diff of 500 files should take less than 10 seconds, took {:?}",
        elapsed
    );
}

/// Test memory efficiency with many states.
#[test]
fn test_memory_efficiency() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create many states
    for i in 0..50 {
        std::fs::write(
            temp.path().join("rotating.txt"),
            format!("large content {}", i),
        )
        .unwrap();
        repo.snapshot(Some(format!("State {}", i)), None).unwrap();
    }

    // 50 user snapshots + 1 bootstrap state from init_default
    let states = repo.store().list_states().unwrap();
    assert_eq!(states.len(), 51);
}

/// Test worktree status performance with many changes.
#[test]
fn test_status_performance_many_changes() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create base state
    for i in 0..100 {
        std::fs::write(
            temp.path().join(format!("file_{}.txt", i)),
            format!("content {}", i),
        )
        .unwrap();
    }
    repo.snapshot(Some("Base".to_string()), None).unwrap();

    // Modify, add, and delete files
    for i in 0..50 {
        // Modify
        std::fs::write(
            temp.path().join(format!("file_{}.txt", i)),
            format!("modified {}", i),
        )
        .unwrap();
    }
    for i in 100..150 {
        // Add new
        std::fs::write(
            temp.path().join(format!("new_{}.txt", i)),
            format!("new {}", i),
        )
        .unwrap();
    }
    for i in 50..75 {
        // Delete
        std::fs::remove_file(temp.path().join(format!("file_{}.txt", i))).unwrap();
    }

    let start = Instant::now();
    let current_state = repo.current_state().unwrap();
    let tree = current_state
        .as_ref()
        .map(|s| repo.store().get_tree(&s.tree).unwrap().unwrap_or_default())
        .unwrap_or_default();
    let status = repo.compare_worktree_cached(&tree).unwrap();
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_secs() < 10,
        "Status with many changes should take less than 10 seconds, took {:?}",
        elapsed
    );

    // Verify counts
    assert!(!status.is_clean());
}

/// Test handling of binary file patterns.
#[test]
fn test_binary_file_patterns() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create various binary-like files
    let files = vec![
        ("image.png", vec![0x89, 0x50, 0x4E, 0x47]), // PNG magic
        ("data.bin", vec![0u8; 1000]),               // Pure binary
        ("archive.zip", vec![0x50, 0x4B, 0x03, 0x04]), // ZIP magic
        ("text.txt", b"plain text".to_vec()),        // Text
    ];

    for (name, content) in files {
        std::fs::write(temp.path().join(name), content).unwrap();
    }

    let result = repo.snapshot(Some("Mixed content".to_string()), None);
    assert!(result.is_ok());
}

/// Test incremental snapshot performance (small changes).
#[test]
fn test_incremental_snapshot_performance() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create base with many files
    for i in 0..1000 {
        std::fs::write(
            temp.path().join(format!("file_{}.txt", i)),
            format!("content {}", i),
        )
        .unwrap();
    }
    repo.snapshot(Some("Base".to_string()), None).unwrap();

    // Make small change
    std::fs::write(temp.path().join("file_0.txt"), "modified").unwrap();

    let start = Instant::now();
    let result = repo.snapshot(Some("Incremental".to_string()), None);
    let elapsed = start.elapsed();

    assert!(result.is_ok());
    // Incremental snapshot should be fast
    assert!(
        elapsed.as_secs() < 5,
        "Incremental snapshot should be fast, took {:?}",
        elapsed
    );
}

#[test]
fn test_native_pack_size_vs_raw_payload() {
    let objects = sample_transport_objects();

    let (pack_data, index_data) = encode_native_pack(&objects);
    let native_size = pack_data.len() + index_data.len();
    let raw_payload_size = objects
        .iter()
        .map(|object| object.data.len())
        .sum::<usize>();
    let ratio = native_size as f64 / raw_payload_size as f64;

    println!(
        "native pack size benchmark: raw_payload={}B native={}B ratio={:.3}",
        raw_payload_size, native_size, ratio
    );

    assert!(
        native_size < raw_payload_size,
        "native pack should beat raw payload size on the benchmark corpus (raw={} native={})",
        raw_payload_size,
        native_size
    );
}

#[test]
#[ignore = "release-build perf budget; run with --include-ignored --release"]
fn test_native_pack_encode_decode_benchmark() {
    let objects = sample_transport_objects();
    let iterations = 10;

    let start = Instant::now();
    for _ in 0..iterations {
        let (pack_data, index_data) = encode_native_pack(&objects);
        let decoded = decode_native_pack(&pack_data, &index_data);
        assert_eq!(decoded.len(), objects.len());
    }
    let native_elapsed = start.elapsed();

    println!(
        "native pack encode/decode benchmark: iterations={} elapsed={:?}",
        iterations, native_elapsed
    );

    assert!(
        native_elapsed < Duration::from_secs(10),
        "native pack encode/decode benchmark exceeded limit: {:?}",
        native_elapsed
    );
}

#[test]
#[ignore = "release-build perf budget; run with --include-ignored --release"]
fn test_fs_pack_build_and_read_benchmark() {
    let objects = sample_fs_pack_objects();
    let mut builder = PackBuilder::new(CompressionConfig::default());
    for (hash, obj_type, data) in &objects {
        builder.add(*hash, *obj_type, data.clone());
    }

    let build_start = Instant::now();
    let (pack_data, index_data, stats) = builder.build().unwrap();
    let build_elapsed = build_start.elapsed();

    let temp = TempDir::new().unwrap();
    let pack_path = temp.path().join("bench.pack");
    let index_path = temp.path().join("bench.idx");
    std::fs::write(&pack_path, &pack_data).unwrap();
    std::fs::write(&index_path, &index_data).unwrap();

    let read_start = Instant::now();
    let reader = objects::store::PackReader::open(&pack_path, &index_path).unwrap();
    for (hash, _, expected) in &objects {
        let (_, data) = reader.get_hashed_object(hash).unwrap().unwrap();
        assert_eq!(&data, expected);
    }
    let read_elapsed = read_start.elapsed();

    println!(
        "fs pack benchmark: objects={} build={:?} read={:?} pack_bytes={} ratio={:.3}",
        stats.object_count,
        build_elapsed,
        read_elapsed,
        pack_data.len(),
        stats.compression_ratio
    );

    assert!(
        build_elapsed < Duration::from_secs(5),
        "fs pack build benchmark exceeded limit: {:?}",
        build_elapsed
    );
    assert!(
        read_elapsed < Duration::from_secs(2),
        "fs pack read benchmark exceeded limit: {:?}",
        read_elapsed
    );
}

fn encode_native_pack(objects: &[ObjectData]) -> (Vec<u8>, Vec<u8>) {
    let mut builder = PackBuilder::new(CompressionConfig::default());
    for object in objects {
        let id = match &object.id {
            ObjectId::Hash(hash) => PackObjectId::Hash(*hash),
            ObjectId::ChangeId(change_id) => PackObjectId::ChangeId(*change_id),
        };
        let obj_type = match object.obj_type {
            ObjectType::Blob => PackObjectType::Blob,
            ObjectType::Tree => PackObjectType::Tree,
            ObjectType::State => PackObjectType::State,
            ObjectType::Action => PackObjectType::Action,
            ObjectType::Redaction => {
                // Redaction sidecars never enter the content-addressed
                // pack; the test fixture doesn't construct them.
                unreachable!("performance harness does not synthesize Redaction objects");
            }
            ObjectType::StateVisibility => {
                // StateVisibility sidecars never enter the content-addressed
                // pack; the test fixture doesn't construct them.
                unreachable!("performance harness does not synthesize StateVisibility objects");
            }
        };
        builder.add_id(id, obj_type, object.data.clone());
    }
    let (pack_data, index_data, _) = builder.build().unwrap();
    (pack_data, index_data)
}

fn decode_native_pack(pack_data: &[u8], index_data: &[u8]) -> Vec<ObjectData> {
    let reader = objects::store::PackReader::from_slice(pack_data, index_data).unwrap();
    reader
        .list_ids()
        .into_iter()
        .map(|id| {
            let (obj_type, data) = reader.get_object(&id).unwrap().unwrap();
            let object_id = match id {
                PackObjectId::Hash(hash) => ObjectId::Hash(hash),
                PackObjectId::ChangeId(change_id) => ObjectId::ChangeId(change_id),
            };
            let object_type = match obj_type {
                PackObjectType::Blob => ObjectType::Blob,
                PackObjectType::Tree => ObjectType::Tree,
                PackObjectType::State => ObjectType::State,
                PackObjectType::Action => ObjectType::Action,
                PackObjectType::Delta => {
                    panic!("decoded native pack should not surface delta type")
                }
            };
            ObjectData {
                id: object_id,
                obj_type: object_type,
                data,
                is_delta: false,
            }
        })
        .collect()
}

fn sample_transport_objects() -> Vec<ObjectData> {
    let repetitive_blob = Blob::new("hello from heddle\n".repeat(4096).into_bytes());
    let source_blob = Blob::new(
        "fn main() { println!(\"hello\"); }\n"
            .repeat(1024)
            .into_bytes(),
    );
    let state_id = ChangeId::generate();

    vec![
        ObjectData {
            id: ObjectId::Hash(repetitive_blob.hash()),
            obj_type: ObjectType::Blob,
            data: repetitive_blob.content().to_vec(),
            is_delta: false,
        },
        ObjectData {
            id: ObjectId::Hash(source_blob.hash()),
            obj_type: ObjectType::Blob,
            data: source_blob.content().to_vec(),
            is_delta: false,
        },
        ObjectData {
            id: ObjectId::ChangeId(state_id),
            obj_type: ObjectType::State,
            data: vec![42; 8 * 1024],
            is_delta: false,
        },
    ]
}

fn sample_fs_pack_objects() -> Vec<(ContentHash, PackObjectType, Vec<u8>)> {
    (0..64)
        .map(|idx| {
            let content =
                format!("module {idx}\n{}\n", "let repeated = 42;\n".repeat(256)).into_bytes();
            let hash = ContentHash::compute(&content);
            (hash, PackObjectType::Blob, content)
        })
        .collect()
}

/// Test empty file handling.
#[test]
fn test_empty_file_handling() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Mix of empty and non-empty files
    std::fs::write(temp.path().join("empty.txt"), "").unwrap();
    std::fs::write(temp.path().join("nonempty.txt"), "content").unwrap();
    std::fs::write(temp.path().join("another_empty.txt"), "").unwrap();

    let result = repo.snapshot(Some("With empty files".to_string()), None);
    assert!(result.is_ok());

    // All files should be tracked
    let state = result.unwrap();
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();
    assert!(tree.get("empty.txt").is_some());
    assert!(tree.get("nonempty.txt").is_some());
    assert!(tree.get("another_empty.txt").is_some());
}

/// Test symlink handling (if supported).
#[test]
#[cfg(unix)]
fn test_symlink_handling() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create file and symlink
    std::fs::write(temp.path().join("target.txt"), "target content").unwrap();
    symlink(temp.path().join("target.txt"), temp.path().join("link.txt")).unwrap();

    let result = repo.snapshot(Some("With symlink".to_string()), None);

    // Behavior depends on implementation - may succeed or fail
    println!("Symlink snapshot result: {:?}", result);
}

/// Test very long filename handling.
#[test]
fn test_long_filename_handling() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create file with very long name (接近系统限制)
    let long_name = "a".repeat(200) + ".txt";
    std::fs::write(temp.path().join(&long_name), "content").unwrap();

    let result = repo.snapshot(Some("Long filename".to_string()), None);
    assert!(result.is_ok());
}

/// Test special characters in filenames.
#[test]
fn test_special_characters_in_filenames() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    let special_names = vec![
        "file with spaces.txt",
        "file-dash.txt",
        "file_underscore.txt",
        "file.multiple.dots.txt",
        "file@symbol.txt",
    ];

    for name in &special_names {
        std::fs::write(temp.path().join(name), format!("content of {}", name)).unwrap();
    }

    let result = repo.snapshot(Some("Special filenames".to_string()), None);
    assert!(result.is_ok());

    let state = result.unwrap();
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();

    for name in &special_names {
        assert!(
            tree.get(name).is_some(),
            "Should handle special filename: {}",
            name
        );
    }
}

// ============================================================================
// Cache Performance Benchmarks
// ============================================================================

/// Test cold cache status performance.
#[test]
fn test_cold_cache_status() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create a realistic directory structure
    for i in 0..50 {
        let dir = temp.path().join(format!("dir{}", i));
        std::fs::create_dir(&dir).unwrap();
        for j in 0..10 {
            std::fs::write(
                dir.join(format!("file{}.txt", j)),
                format!("content {} {}", i, j),
            )
            .unwrap();
        }
    }

    // Create initial snapshot to populate tree
    repo.snapshot(Some("Initial".to_string()), None).unwrap();

    // Delete the index file to simulate cold cache
    let index_path = temp.path().join(".heddle/state").join("index.bin");
    let _ = std::fs::remove_file(&index_path);

    // Measure cold cache status time
    let start = Instant::now();
    let current_state = repo.current_state().unwrap();
    let tree = current_state
        .as_ref()
        .map(|s| repo.store().get_tree(&s.tree).unwrap().unwrap_or_default())
        .unwrap_or_default();
    let status = repo.compare_worktree_cached(&tree).unwrap();
    let cold_time = start.elapsed();

    println!("Cold cache status: {:?}", cold_time);
    assert!(status.is_clean());

    // Cold cache should still be reasonable
    assert!(
        cold_time.as_secs() < 30,
        "Cold cache status should complete in under 30 seconds, took {:?}",
        cold_time
    );
}

/// Test warm cache status performance.
#[test]
fn test_warm_cache_status() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create a realistic directory structure
    for i in 0..50 {
        let dir = temp.path().join(format!("dir{}", i));
        std::fs::create_dir(&dir).unwrap();
        for j in 0..10 {
            std::fs::write(
                dir.join(format!("file{}.txt", j)),
                format!("content {} {}", i, j),
            )
            .unwrap();
        }
    }

    // Create initial snapshot (this populates the cache)
    repo.snapshot(Some("Initial".to_string()), None).unwrap();

    // First status run to warm up any in-memory caches
    let current_state = repo.current_state().unwrap();
    let tree = current_state
        .as_ref()
        .map(|s| repo.store().get_tree(&s.tree).unwrap().unwrap_or_default())
        .unwrap_or_default();
    let _status = repo.compare_worktree_cached(&tree).unwrap();

    // Measure warm cache status time
    let start = Instant::now();
    let current_state = repo.current_state().unwrap();
    let tree = current_state
        .as_ref()
        .map(|s| repo.store().get_tree(&s.tree).unwrap().unwrap_or_default())
        .unwrap_or_default();
    let status = repo.compare_worktree_cached(&tree).unwrap();
    let warm_time = start.elapsed();

    println!("Warm cache status: {:?}", warm_time);
    assert!(status.is_clean());

    // Should be fast
    assert!(
        warm_time.as_secs() < 10,
        "Warm cache status should complete in under 10 seconds, took {:?}",
        warm_time
    );
}

/// Test that single file change detection is O(changed_files), not O(repo size).
#[test]
fn test_single_file_change_detection() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create a large repository with many files
    let file_count = 500;
    for i in 0..file_count {
        let dir = temp.path().join(format!("dir{}", i % 10));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("file{}.txt", i)), format!("content {}", i)).unwrap();
    }

    // Create initial snapshot
    repo.snapshot(Some("Initial".to_string()), None).unwrap();

    // Modify just one file
    std::fs::write(
        temp.path().join("dir0").join("file0.txt"),
        "modified content",
    )
    .unwrap();

    // Measure time for status with one change
    let start = Instant::now();
    let current_state = repo.current_state().unwrap();
    let tree = current_state
        .as_ref()
        .map(|s| repo.store().get_tree(&s.tree).unwrap().unwrap_or_default())
        .unwrap_or_default();
    let status = repo.compare_worktree_cached(&tree).unwrap();
    let detection_time = start.elapsed();

    println!(
        "Single file change detection: {:?} ({} files in repo)",
        detection_time, file_count
    );

    // Should detect the single change
    assert!(!status.is_clean(), "Should detect modified file");
    assert_eq!(
        status.modified.len(),
        1,
        "Should have exactly one modified file"
    );

    // Detection should be reasonably fast even with many files
    // With proper caching, it should be close to O(changed_files)
    assert!(
        detection_time.as_secs() < 15,
        "Single file change detection should be fast, took {:?}",
        detection_time
    );
}
