// SPDX-License-Identifier: Apache-2.0
#![cfg(feature = "fs")]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    fs::{File, OpenOptions},
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use objects::{
    object::{
        Attribution, Blob, ContentHash, ObjectSource, Principal, State, StateId, Tree, TreeEntry,
    },
    store::{
        FsStore, ObjectStore, Result,
        pack::{ObjectType, PackObjectId, PackReader, StreamingPackBuilder, build_source_pack},
    },
};

struct CountingAllocator;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static RUN: Mutex<()> = Mutex::new(());

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) };
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let result = unsafe { System.realloc(pointer, layout, size) };
        if !result.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        result
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn builder(root: &Path, name: &str) -> StreamingPackBuilder<File> {
    StreamingPackBuilder::new(
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(root.join(format!("{name}.pack")))
            .expect("pack"),
        root.join(format!("{name}.idx")),
        heddle_format::compression::CompressionConfig::disabled(),
        root.join(format!("{name}-buckets")),
    )
    .expect("builder")
}

// Three directory levels, at most 100 entries per directory. Fixture setup
// streams directly to disk and retains only the top two directory levels.
fn fixture(root: &Path, count: usize) -> State {
    let mut output = builder(root, "input");
    let mut top = Vec::new();
    let mut middle = Vec::new();
    for directory in 0..count.div_ceil(100) {
        let mut files = Vec::new();
        for index in directory * 100..((directory + 1) * 100).min(count) {
            let bytes = (index as u64).to_be_bytes();
            let hash = ContentHash::compute_typed("blob", &bytes);
            output
                .add_id(PackObjectId::Hash(hash), ObjectType::Blob, bytes)
                .expect("blob");
            files.push(TreeEntry::file(format!("f{index}"), hash, false).expect("entry"));
        }
        let tree = Tree::from_entries(files);
        output
            .add_id(
                PackObjectId::Hash(tree.hash()),
                ObjectType::Tree,
                tree.encode_canonical().expect("tree"),
            )
            .expect("tree record");
        middle.push(TreeEntry::directory(format!("d{directory}"), tree.hash()).expect("directory"));
        if middle.len() == 100 || directory + 1 == count.div_ceil(100) {
            let tree = Tree::from_entries(std::mem::take(&mut middle));
            output
                .add_id(
                    PackObjectId::Hash(tree.hash()),
                    ObjectType::Tree,
                    tree.encode_canonical().expect("tree"),
                )
                .expect("middle record");
            top.push(
                TreeEntry::directory(format!("m{directory}"), tree.hash()).expect("directory"),
            );
        }
    }
    let tree = Tree::from_entries(top);
    output
        .add_id(
            PackObjectId::Hash(tree.hash()),
            ObjectType::Tree,
            tree.encode_canonical().expect("root"),
        )
        .expect("root record");
    let state = State::new_snapshot(
        tree.hash(),
        vec![],
        Attribution::human(Principal::new("owner", "owner@example.test")),
    );
    output
        .add_id(
            PackObjectId::StateId(state.id()),
            ObjectType::State,
            state.encode_current_msgpack().expect("state"),
        )
        .expect("state record");
    output.finalize().expect("fixture pack");
    state
}

struct Source(PackReader<'static>);
impl ObjectSource for Source {
    fn get_tree(&self, hash: &ContentHash) -> Result<Option<Tree>> {
        Ok(self
            .0
            .get_hashed_object(hash)?
            .map(|(kind, bytes)| {
                assert_eq!(kind, ObjectType::Tree);
                Tree::decode_canonical(&bytes)
            })
            .transpose()?)
    }
    fn get_blob(&self, hash: &ContentHash) -> Result<Option<Blob>> {
        Ok(self.0.get_hashed_object(hash)?.map(|(kind, bytes)| {
            assert_eq!(kind, ObjectType::Blob);
            Blob::new(bytes)
        }))
    }
    fn decoded_blob_len(&self, hash: &ContentHash) -> Result<Option<u64>> {
        self.0.get_hashed_object_size(hash)
    }
    fn get_state(&self, _: &StateId) -> Result<Option<State>> {
        panic!("Fetch must not walk source history")
    }
}

fn pipeline(count: usize, measure_only: bool) {
    let _run = RUN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().expect("fixture");
    let state = fixture(directory.path(), count);
    let store = FsStore::new(directory.path().join("destination"));
    store.init().expect("store");
    let baseline = LIVE.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    let start = Instant::now();
    let source = Source(
        PackReader::open(
            &directory.path().join("input.pack"),
            &directory.path().join("input.idx"),
            directory.path(),
        )
        .expect("source"),
    );
    let (_, stats) = build_source_pack(
        builder(directory.path(), "source"),
        &source,
        &state,
        256 * 1024 * 1024,
    )
    .expect("prepare source using byte budget");
    let prepare = start.elapsed();
    let reader = PackReader::open(
        &directory.path().join("source.pack"),
        &directory.path().join("source.idx"),
        directory.path(),
    )
    .expect("reader");
    reader
        .validate_source_closure(&state, 256 * 1024 * 1024)
        .expect("validate exact source");
    let validate = start.elapsed() - prepare;
    drop(reader);
    let install_start = Instant::now();
    let installed = store
        .install_pack_streaming(
            &directory.path().join("source.pack"),
            &directory.path().join("source.idx"),
        )
        .expect("install source");
    assert_eq!(installed.len() as u64, stats.object_count);
    assert_eq!(
        ObjectStore::get_state(&store, &state.id()).expect("state read"),
        Some(state)
    );
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(baseline);
    let rss = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let rss = rss
        .lines()
        .find(|line| line.starts_with("VmHWM:"))
        .unwrap_or("RSS unavailable");
    eprintln!(
        "Fetch objects={} prepare={prepare:?} validate={validate:?} install={:?} total={:?} peak_allocated={} bytes {rss}",
        stats.object_count,
        install_start.elapsed(),
        start.elapsed(),
        peak
    );
    #[cfg(target_os = "linux")]
    if !measure_only {
        let peak_rss_kib: usize = rss
            .split_whitespace()
            .nth(1)
            .expect("peak RSS")
            .parse()
            .expect("RSS in KiB");
        assert!(
            peak_rss_kib < 128 * 1024,
            "peak RSS {peak_rss_kib} KiB exceeds fixed 128 MiB bound"
        );
    }
    assert!(
        peak < 16 * 1024 * 1024,
        "Fetch allocated {peak} bytes; fixed bound is 16 MiB"
    );
}

#[test]
fn fetch_250k_source_uses_byte_budget() {
    let _run = RUN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().expect("fixture");
    let state = fixture(directory.path(), 247_498);
    let source = Source(
        PackReader::open(
            &directory.path().join("input.pack"),
            &directory.path().join("input.idx"),
            directory.path(),
        )
        .expect("source"),
    );
    assert_eq!(source.0.object_count(), 250_000);
    let mut decoded_bytes = 0_u64;
    source
        .0
        .visit_objects(|_, _, bytes| {
            decoded_bytes += bytes.len() as u64;
            Ok(())
        })
        .expect("measure decoded source");
    let store = FsStore::new(directory.path().join("destination"));
    store.init().expect("store");
    let result = (|| -> Result<()> {
        let scratch =
            objects::store::pack::ScratchDir::new(&store.root().join("tmp"), "fetch-byte-budget-")?;
        build_source_pack(
            builder(scratch.path(), "source"),
            &source,
            &state,
            decoded_bytes - 1,
        )?;
        store.install_pack_streaming(
            &scratch.path().join("source.pack"),
            &scratch.path().join("source.idx"),
        )?;
        Ok(())
    })();
    assert!(
        matches!(result, Err(objects::store::HeddleError::InvalidObject(ref message)) if message == "source pack decoded byte budget exceeded"),
        "{result:?}"
    );
    assert!(store.list_blobs().expect("blobs").is_empty());
    assert!(store.list_trees().expect("trees").is_empty());
    assert!(store.list_states().expect("states").is_empty());
    assert_eq!(
        std::fs::read_dir(store.root().join("packs"))
            .expect("packs")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "pack"))
            .count(),
        0
    );
    assert_eq!(
        std::fs::read_dir(store.root().join("tmp"))
            .expect("scratch")
            .count(),
        0
    );
}

#[test]
fn fetch_250k_source_memory_is_bounded() {
    pipeline(247_498, false);
}

#[test]
fn fetch_100k_source_memory_is_bounded() {
    pipeline(98_998, false);
}

#[test]
fn fetch_adversarial_index_sort_is_bounded() {
    let _run = RUN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().expect("sort fixture");
    let baseline = LIVE.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    let mut output = builder(directory.path(), "one-prefix");
    for index in (0..250_000_u64).rev() {
        let mut id = [0; 32];
        id[24..].copy_from_slice(&index.to_be_bytes());
        output
            .add_id(
                PackObjectId::StateId(StateId::from_bytes(id)),
                ObjectType::State,
                index.to_be_bytes(),
            )
            .expect("index record");
    }
    output.finalize().expect("sort one adversarial bucket");
    let reader = PackReader::open(
        &directory.path().join("one-prefix.pack"),
        &directory.path().join("one-prefix.idx"),
        directory.path(),
    )
    .expect("sorted index");
    assert_eq!(reader.object_count(), 250_000);
    for index in [0_u64, 125_000, 249_999] {
        let mut id = [0; 32];
        id[24..].copy_from_slice(&index.to_be_bytes());
        assert!(
            reader
                .has_object(&PackObjectId::StateId(StateId::from_bytes(id)))
                .expect("point lookup")
        );
    }
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(baseline);
    assert!(
        peak < 4 * 1024 * 1024,
        "one-prefix index allocated {peak} bytes"
    );
}

#[test]
fn fetch_rejects_truncated_extra_and_tampered_packs() {
    let _run = RUN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().expect("corruption fixture");
    let state = fixture(directory.path(), 200);
    let input = PackReader::open(
        &directory.path().join("input.pack"),
        &directory.path().join("input.idx"),
        directory.path(),
    )
    .expect("fixture reader");
    let store = FsStore::new(directory.path().join("destination"));
    store.init().expect("destination");
    for mode in ["truncated", "extra", "tampered"] {
        let mut output = builder(directory.path(), mode);
        let mut changed = false;
        input
            .visit_objects(|id, kind, bytes| {
                if mode == "tampered" && kind == ObjectType::Blob && !changed {
                    let mut bytes = bytes.to_vec();
                    bytes[0] ^= 1;
                    changed = true;
                    output.add_id(id, kind, bytes)?;
                } else {
                    output.add_id(id, kind, bytes)?;
                }
                Ok(())
            })
            .expect("copy records");
        if mode == "extra" {
            let blob = Blob::new(b"unselected private source".to_vec());
            output
                .add_id(
                    PackObjectId::Hash(blob.hash()),
                    ObjectType::Blob,
                    blob.content(),
                )
                .expect("extra valid blob");
        }
        output.finalize().expect("checksummed pack");
        let pack = directory.path().join(format!("{mode}.pack"));
        let index = directory.path().join(format!("{mode}.idx"));
        if mode == "truncated" {
            let file = OpenOptions::new()
                .write(true)
                .open(&pack)
                .expect("truncate");
            file.set_len(file.metadata().expect("size").len() - 9)
                .expect("truncated trailer");
        }
        let result = (|| -> Result<()> {
            let reader = PackReader::open(&pack, &index, directory.path())?;
            reader.validate_source_closure(&state, 256 * 1024 * 1024)?;
            drop(reader);
            store.install_pack_streaming(&pack, &index)?;
            Ok(())
        })();
        assert!(result.is_err(), "Fetch admitted a {mode} pack");
        assert!(!ObjectStore::has_state(&store, &state.id()).expect("store stays empty"));
    }
}

#[test]
fn streaming_install_rejects_duplicate_ids_hiding_tampered_records() {
    let _run = RUN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().expect("duplicate fixture");
    let blob = Blob::new(b"canonical object".to_vec());
    let mut output = builder(directory.path(), "duplicate");
    output
        .add_id(
            PackObjectId::Hash(blob.hash()),
            ObjectType::Blob,
            b"tampered object",
        )
        .expect("tampered record");
    output
        .add_id(
            PackObjectId::Hash(blob.hash()),
            ObjectType::Blob,
            blob.content(),
        )
        .expect("valid record under the same identity");
    output.finalize().expect("checksummed duplicate pack");
    let store = FsStore::new(directory.path().join("destination"));
    store.init().expect("store");
    assert!(
        store
            .install_pack_streaming(
                &directory.path().join("duplicate.pack"),
                &directory.path().join("duplicate.idx"),
            )
            .is_err(),
        "a valid copy must not hide a tampered physical record"
    );
    assert!(
        !store
            .has_blob_locally(&blob.hash())
            .expect("store stays empty")
    );
}

// Opt-in measurement, not part of the regression gate. Run with --release
// --ignored --exact fetch_1m_source_release_timing --nocapture.
#[test]
#[ignore = "one-million-object release timing on the shared build host"]
fn fetch_1m_source_release_timing() {
    pipeline(989_999, true);
}

#[test]
fn source_closure_rejects_a_tree_referenced_as_a_blob() {
    let _run = RUN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().expect("fixture");
    let store = FsStore::new(directory.path().join("destination"));
    store.init().expect("store");
    let tree_as_blob = Tree::new();
    let root = Tree::from_entries(vec![
        TreeEntry::file("wrong-type", tree_as_blob.hash(), false).expect("entry"),
    ]);
    let state = State::new_snapshot(
        root.hash(),
        vec![],
        Attribution::human(Principal::new("owner", "owner@example.test")),
    );
    let result = (|| -> Result<()> {
        let scratch =
            objects::store::pack::ScratchDir::new(&store.root().join("tmp"), "wrong-type-")?;
        let mut output = builder(scratch.path(), "source");
        for tree in [&root, &tree_as_blob] {
            output.add_id(
                PackObjectId::Hash(tree.hash()),
                ObjectType::Tree,
                tree.encode_canonical()?,
            )?;
        }
        output.add_id(
            PackObjectId::StateId(state.id()),
            ObjectType::State,
            state.encode_current_msgpack()?,
        )?;
        output.finalize()?;
        let reader = PackReader::open(
            &scratch.path().join("source.pack"),
            &scratch.path().join("source.idx"),
            scratch.path(),
        )?;
        reader.validate_source_closure(&state, 256 * 1024 * 1024)?;
        store.install_pack_streaming(
            &scratch.path().join("source.pack"),
            &scratch.path().join("source.idx"),
        )?;
        Ok(())
    })();
    assert!(
        matches!(result, Err(objects::store::HeddleError::InvalidObject(ref message)) if message.contains("type mismatch")),
        "{result:?}"
    );
    assert!(store.list_states().expect("empty store").is_empty());
    assert!(store.list_trees().expect("empty store").is_empty());
    assert_eq!(
        std::fs::read_dir(store.root().join("tmp"))
            .expect("scratch")
            .count(),
        0
    );
}

#[test]
fn source_closure_rejects_duplicate_ids_and_cleans_failed_fetch_scratch() {
    let _run = RUN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().expect("fixture");
    let state = fixture(directory.path(), 1);
    let input = PackReader::open(
        &directory.path().join("input.pack"),
        &directory.path().join("input.idx"),
        directory.path(),
    )
    .expect("reader");
    let store = FsStore::new(directory.path().join("destination"));
    store.init().expect("store");
    let result = (|| -> Result<()> {
        let scratch =
            objects::store::pack::ScratchDir::new(&store.root().join("tmp"), "duplicate-fetch-")?;
        let mut output = builder(scratch.path(), "source");
        input.visit_objects(|id, kind, bytes| {
            output.add_id(id, kind, bytes)?;
            if kind == ObjectType::Blob {
                output.add_id(id, kind, bytes)?;
            }
            Ok(())
        })?;
        output.finalize()?;
        let reader = PackReader::open(
            &scratch.path().join("source.pack"),
            &scratch.path().join("source.idx"),
            scratch.path(),
        )?;
        reader.validate_source_closure(&state, 256 * 1024 * 1024)?;
        store.install_pack_streaming(
            &scratch.path().join("source.pack"),
            &scratch.path().join("source.idx"),
        )?;
        Ok(())
    })();
    assert!(
        matches!(result, Err(objects::store::HeddleError::InvalidObject(ref message)) if message.contains("repeated") || message.contains("duplicate")),
        "{result:?}"
    );
    assert!(store.list_states().expect("empty store").is_empty());
    assert!(store.list_blobs().expect("empty store").is_empty());
    assert_eq!(
        std::fs::read_dir(store.root().join("tmp"))
            .expect("scratch")
            .count(),
        0
    );
}
