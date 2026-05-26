// SPDX-License-Identifier: Apache-2.0
//! Unit tests for the content-addressed mount core.
//!
//! These exercise [`ContentAddressedMount`] directly, without going
//! through any platform shell. They use the same `Repository` fixture
//! that `crates/repo` uses in its own tests: an init-default repo in
//! a tempdir, with files written into the worktree and snapshotted
//! to advance `main`.

use std::{
    ffi::OsStr,
    fs,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use objects::{
    error::HeddleError,
    object::{Action, ActionId, Blob, ChangeId, ContentHash, State, ThreadName, Tree},
    store::ObjectStore,
};
use repo::Repository;
use tempfile::TempDir;

use crate::{
    core::ContentAddressedMount,
    error::MountError,
    shell::{NodeId, NodeKind, PlatformShell},
};

/// Shared test mocks. Lives under `tests::mocks` so the per-platform
/// adapter unit tests (FUSE on Linux, FSKit on macOS, ProjFS on
/// Windows) can reuse the same in-memory shell without duplicating
/// it inline.
///
/// Gated on the features that actually consume the mocks so OSS-only
/// builds (no `fuse` / `fskit` / `projfs`) don't trip clippy's
/// `-D warnings` on dead test code.
#[cfg(any(
    all(target_os = "linux", feature = "fuse"),
    all(target_os = "macos", feature = "fskit"),
    all(target_os = "windows", feature = "projfs"),
))]
pub(crate) mod mocks {
    use std::{
        ffi::OsStr,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::UNIX_EPOCH,
    };

    use crate::{
        error::{MountError, Result},
        shell::{Attrs, Entry, NodeId, NodeKind, PlatformShell, DIR_UNIX_MODE},
    };

    /// Trivial in-memory shell that lets adapter unit tests validate
    /// the session construct-and-drop lifecycle without needing a real
    /// `ContentAddressedMount` (which requires a `Repository`).
    ///
    /// Increments `drops` exactly once when the shell is finally
    /// dropped, so a test that boxes the shell into a C ABI handle can
    /// assert the box was reclaimed.
    ///
    /// `dead_code` is silenced because this is only consumed by the
    /// FSKit / ProjFS unit tests — the FUSE shell tests use
    /// [`PanicShell`] but never [`CountingShell`], and clippy's
    /// `-D warnings` would otherwise fail the Linux+fuse build.
    #[allow(dead_code)]
    pub struct CountingShell {
        pub drops: Arc<AtomicUsize>,
    }

    #[allow(dead_code)]
    impl CountingShell {
        pub fn new() -> (Self, Arc<AtomicUsize>) {
            let drops = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    drops: Arc::clone(&drops),
                },
                drops,
            )
        }
    }

    impl Drop for CountingShell {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl PlatformShell for CountingShell {
        fn lookup(&self, _parent: NodeId, _name: &OsStr) -> Result<Option<Entry>> {
            Ok(None)
        }
        fn read(&self, _node: NodeId, _offset: u64, _buf: &mut [u8]) -> Result<usize> {
            Ok(0)
        }
        fn write(&self, _node: NodeId, _offset: u64, _data: &[u8]) -> Result<usize> {
            Err(MountError::ReadOnly)
        }
        fn enumerate(&self, _dir: NodeId) -> Result<Vec<Entry>> {
            Ok(vec![])
        }
        fn attrs(&self, node: NodeId) -> Result<Attrs> {
            Ok(Attrs {
                node,
                kind: NodeKind::Directory,
                size: 0,
                unix_mode: DIR_UNIX_MODE,
                nlink: 2,
                mtime: UNIX_EPOCH,
            })
        }
        fn invalidate(&self, _node: NodeId) -> Result<()> {
            Ok(())
        }
    }

    /// Shell that panics on every PlatformShell call. Used by the
    /// FFI panic-resilience tests to drive an unwind into a
    /// trampoline body — the `catch_unwind` wrappers in
    /// `fskit::guarded_c_int` and `projfs::guarded_hresult` must
    /// translate the panic into `EIO` / a Win32 I/O HRESULT instead
    /// of letting the unwind cross the C ABI boundary (which Rust
    /// ≥1.81 turns into an abort that would crash the host process
    /// and every projected/materialised volume with it).
    ///
    /// `dead_code` is silenced because not every feature flag pulls
    /// in a consumer: the Linux+fuse build uses it via
    /// `fuse::tests::guard_call_translates_panic_to_eio`, but a
    /// Windows+projfs-only build wires `guarded_hresult` through its
    /// closure tests (no PanicShell required), so the type sits
    /// dormant on that feature set. clippy's `-D warnings` would
    /// otherwise break the Windows ProjFS clippy gate.
    #[allow(dead_code)]
    pub struct PanicShell;

    impl PlatformShell for PanicShell {
        fn lookup(&self, _parent: NodeId, _name: &OsStr) -> Result<Option<Entry>> {
            panic!("panic-shell: lookup intentionally panics")
        }
        fn read(&self, _node: NodeId, _offset: u64, _buf: &mut [u8]) -> Result<usize> {
            panic!("panic-shell: read intentionally panics")
        }
        fn write(&self, _node: NodeId, _offset: u64, _data: &[u8]) -> Result<usize> {
            panic!("panic-shell: write intentionally panics")
        }
        fn enumerate(&self, _dir: NodeId) -> Result<Vec<Entry>> {
            panic!("panic-shell: enumerate intentionally panics")
        }
        fn attrs(&self, _node: NodeId) -> Result<Attrs> {
            panic!("panic-shell: attrs intentionally panics")
        }
        fn invalidate(&self, _node: NodeId) -> Result<()> {
            panic!("panic-shell: invalidate intentionally panics")
        }
    }
}

/// Build a repository with a small, deterministic tree:
///
/// ```text
/// hello.txt    "world"
/// nested/
///   inner.txt  "deep"
///   note.md    "# heading\nbody\n"
/// run.sh       executable, "#!/bin/sh\n"
/// ```
fn fixture() -> (TempDir, Repository) {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    fs::write(temp.path().join("hello.txt"), b"world").unwrap();
    fs::create_dir_all(temp.path().join("nested")).unwrap();
    fs::write(temp.path().join("nested/inner.txt"), b"deep").unwrap();
    fs::write(temp.path().join("nested/note.md"), b"# heading\nbody\n").unwrap();
    let run_path = temp.path().join("run.sh");
    fs::write(&run_path, b"#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&run_path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&run_path, perms).unwrap();
    }
    repo.snapshot(Some("fixture".into()), None).unwrap();
    (temp, repo)
}

fn open_mount() -> (TempDir, ContentAddressedMount) {
    let (temp, repo) = fixture();
    let mount = ContentAddressedMount::new(repo, "main").unwrap();
    (temp, mount)
}

#[test]
fn lookup_hits_root_entry() {
    let (_temp, mount) = open_mount();
    let entry = mount
        .lookup(NodeId::ROOT, OsStr::new("hello.txt"))
        .unwrap()
        .expect("hello.txt should exist");
    assert_eq!(entry.kind, NodeKind::File);
    assert_eq!(entry.size, 5);
    assert_eq!(entry.unix_mode & 0o777, 0o644);
}

#[test]
fn lookup_misses_return_none() {
    let (_temp, mount) = open_mount();
    let missing = mount
        .lookup(NodeId::ROOT, OsStr::new("does-not-exist"))
        .unwrap();
    assert!(missing.is_none());
}

#[test]
fn read_full_file() {
    let (_temp, mount) = open_mount();
    let node = mount.lookup_path("hello.txt").unwrap();
    let mut buf = vec![0u8; 64];
    let n = mount.read(node, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"world");
}

#[test]
fn read_with_offset_returns_tail() {
    let (_temp, mount) = open_mount();
    let node = mount.lookup_path("nested/note.md").unwrap();
    let mut buf = vec![0u8; 64];
    let n = mount.read(node, 10, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"body\n");
}

#[test]
fn read_past_eof_yields_zero() {
    let (_temp, mount) = open_mount();
    let node = mount.lookup_path("hello.txt").unwrap();
    let mut buf = vec![0u8; 16];
    let n = mount.read(node, 9_999, &mut buf).unwrap();
    assert_eq!(n, 0);
}

#[test]
fn enumerate_root_lists_all_entries() {
    let (_temp, mount) = open_mount();
    let entries = mount.enumerate(NodeId::ROOT).unwrap();
    let names: Vec<String> = entries
        .iter()
        .map(|e| e.name.to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"hello.txt".to_string()));
    assert!(names.contains(&"nested".to_string()));
    assert!(names.contains(&"run.sh".to_string()));
}

#[test]
fn enumerate_nested_lists_subdir_entries() {
    let (_temp, mount) = open_mount();
    let nested = mount.lookup_path("nested").unwrap();
    let entries = mount.enumerate(nested).unwrap();
    let names: Vec<_> = entries
        .iter()
        .map(|e| e.name.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        {
            let mut sorted = names.clone();
            sorted.sort();
            sorted
        },
        vec!["inner.txt".to_string(), "note.md".to_string()]
    );
}

#[test]
fn attrs_distinguish_file_and_directory() {
    let (_temp, mount) = open_mount();

    let root_attrs = mount.attrs(NodeId::ROOT).unwrap();
    assert_eq!(root_attrs.kind, NodeKind::Directory);
    assert_eq!(root_attrs.unix_mode & 0o170000, 0o040000);
    assert!(root_attrs.size >= 3); // at least the three top-level entries

    let file = mount.lookup_path("hello.txt").unwrap();
    let file_attrs = mount.attrs(file).unwrap();
    assert_eq!(file_attrs.kind, NodeKind::File);
    assert_eq!(file_attrs.size, 5);
    assert_eq!(file_attrs.nlink, 1);
}

#[test]
fn attrs_preserve_executable_bit() {
    let (_temp, mount) = open_mount();
    let run = mount.lookup_path("run.sh").unwrap();
    let attrs = mount.attrs(run).unwrap();
    assert_eq!(attrs.unix_mode & 0o111, 0o111);
}

#[test]
fn write_to_overlay_then_read_back() {
    // Two-tier write: a write against a captured `File` NodeId
    // mints a hot-tier buffer keyed off the file's path. Subsequent
    // reads through that NodeId serve from the buffer (read-after-
    // write consistency in the same FUSE session).
    let (_temp, mount) = open_mount();
    let node = mount.lookup_path("hello.txt").unwrap();
    let written = mount.write(node, 0, b"WORLD").unwrap();
    assert_eq!(written, 5);
    // Hot buffer should be populated; warm tier still empty.
    assert_eq!(mount.hot_buffer_count(), 1);
    assert!(mount.warm_keys().is_empty());

    // Read through the same NodeId serves from the hot buffer.
    let mut buf = vec![0u8; 16];
    let n = mount.read(node, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"WORLD");
}

// ---------------------------------------------------------------------------
// POSIX `pwrite` semantics: partial overwrites preserve the captured
// tail; offsets past EOF zero-fill. These exercise the fix for the
// "fresh hot buffer" bug — without the seed-from-durable-source step,
// any write less than the full file length would silently truncate
// on flush.
// ---------------------------------------------------------------------------

/// Build a mount whose captured tree contains a single file with the
/// given content. Useful for tests that want to assert against the
/// shape of a captured blob after a partial overwrite.
fn mount_with_seed(path: &str, content: &[u8]) -> (TempDir, ContentAddressedMount) {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let full = temp.path().join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&full, content).unwrap();
    repo.snapshot(Some("seed".into()), None).unwrap();
    let mount = ContentAddressedMount::new(repo, "main").unwrap();
    (temp, mount)
}

/// Read the blob bytes for `path` from the captured tree at `change_id`.
fn read_captured_blob(mount: &ContentAddressedMount, change_id: &ChangeId, path: &str) -> Vec<u8> {
    let store = mount.repo_handle().store();
    let state = store.get_state(change_id).unwrap().unwrap();
    let mut tree = store.get_tree(&state.tree).unwrap().unwrap();
    let comps: Vec<&str> = std::path::Path::new(path)
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(n) => n.to_str(),
            _ => None,
        })
        .collect();
    let (leaf, dirs) = comps.split_last().expect("non-empty path");
    for dir in dirs {
        let entry = tree.get(dir).expect("intermediate dir");
        assert!(entry.is_tree());
        tree = store.get_tree(&entry.hash).unwrap().unwrap();
    }
    let entry = tree.get(leaf).expect("leaf entry");
    let blob = store.get_blob(&entry.hash).unwrap().unwrap();
    blob.into_content()
}

#[test]
fn partial_overwrite_preserves_captured_tail() {
    // Write "HELLO" at offset 0 over a captured file of "hello world\n".
    // POSIX semantics: only the first 5 bytes change; the trailing
    // " world\n" is preserved.
    let (_temp, mount) = mount_with_seed("greet.txt", b"hello world\n");
    let node = mount.lookup_path("greet.txt").unwrap();
    mount.write(node, 0, b"HELLO").unwrap();
    mount.flush(node).unwrap();
    let new_id = mount.capture(Some("partial overwrite".into())).unwrap();
    let bytes = read_captured_blob(&mount, &new_id, "greet.txt");
    assert_eq!(bytes, b"HELLO world\n");
}

#[test]
fn partial_overwrite_at_nonzero_offset() {
    // Write "XYZ" at offset 3 over "abcdefgh". Result: "abcXYZgh".
    let (_temp, mount) = mount_with_seed("alpha.txt", b"abcdefgh");
    let node = mount.lookup_path("alpha.txt").unwrap();
    mount.write(node, 3, b"XYZ").unwrap();
    mount.flush(node).unwrap();
    let new_id = mount.capture(Some("middle overwrite".into())).unwrap();
    let bytes = read_captured_blob(&mount, &new_id, "alpha.txt");
    assert_eq!(bytes, b"abcXYZgh");
}

#[test]
fn write_past_end_zero_fills() {
    // Write "XYZ" at offset 10 over "abc" (len 3). POSIX `pwrite`
    // zero-fills the gap: result is "abc" + 7 NULs + "XYZ", total 13.
    let (_temp, mount) = mount_with_seed("short.txt", b"abc");
    let node = mount.lookup_path("short.txt").unwrap();
    mount.write(node, 10, b"XYZ").unwrap();
    mount.flush(node).unwrap();
    let new_id = mount.capture(Some("zero fill".into())).unwrap();
    let bytes = read_captured_blob(&mount, &new_id, "short.txt");
    assert_eq!(bytes, b"abc\0\0\0\0\0\0\0XYZ");
    assert_eq!(bytes.len(), 13);
}

#[test]
fn write_seeds_from_warm_tier_not_captured() {
    // Captured: "original" (8 bytes). First write replaces it with
    // "FIRST_VERSION" (13 bytes) and flushes — the warm tier now
    // holds the 13-byte version. A second write of "X" at offset 0
    // must seed from the warm tier, not from the captured "original".
    // Expected result: "XIRST_VERSION", NOT "Xriginal".
    let (_temp, mount) = mount_with_seed("evolving.txt", b"original");
    let node = mount.lookup_path("evolving.txt").unwrap();
    mount.write(node, 0, b"FIRST_VERSION").unwrap();
    mount.flush(node).unwrap();
    // Hot buffer drained; warm tier holds "FIRST_VERSION".
    assert_eq!(mount.hot_buffer_count(), 0);
    assert!(mount.warm_blob("evolving.txt").is_some());

    // Second write at the same path. Re-resolve via lookup so the
    // platform shell goes through its normal warm-tier overlay.
    let node2 = mount.lookup_path("evolving.txt").unwrap();
    mount.write(node2, 0, b"X").unwrap();
    mount.flush(node2).unwrap();
    let new_id = mount.capture(Some("warm seed".into())).unwrap();
    let bytes = read_captured_blob(&mount, &new_id, "evolving.txt");
    assert_eq!(bytes, b"XIRST_VERSION");
}

#[test]
fn empty_buffer_for_new_file_path() {
    // A path that doesn't exist in the captured tree at all. Write
    // at offset 5 with "hi" — the gap [0, 5) zero-fills, producing
    // "\0\0\0\0\0hi".
    let (_temp, mount) = fresh_mount();
    let node = create_pending_file(&mount, "brand_new.txt", objects::object::FileMode::Normal);
    mount.write(node, 5, b"hi").unwrap();
    mount.flush(node).unwrap();
    let new_id = mount.capture(Some("brand new".into())).unwrap();
    let bytes = read_captured_blob(&mount, &new_id, "brand_new.txt");
    assert_eq!(bytes, b"\0\0\0\0\0hi");
}

#[test]
fn write_to_directory_returns_read_only() {
    let (_temp, mount) = open_mount();
    let err = mount.write(NodeId::ROOT, 0, b"x").unwrap_err();
    assert!(matches!(err, MountError::ReadOnly));
}

#[test]
fn unknown_thread_is_enoent_shaped() {
    let (_temp, repo) = fixture();
    let err = match ContentAddressedMount::new(repo, "no-such-thread") {
        Ok(_) => panic!("expected unknown-thread error"),
        Err(err) => err,
    };
    assert!(matches!(err, MountError::UnknownThread(_)));
    assert_eq!(err.to_errno(), libc::ENOENT);
}

#[test]
fn invalidate_drops_the_mapping() {
    let (_temp, mount) = open_mount();
    let node = mount.lookup_path("hello.txt").unwrap();
    mount.invalidate(node).unwrap();
    // Re-lookup should hand back a fresh (potentially equal) NodeId
    // and not be stale. Reading the freshly-handed-out id should
    // succeed.
    let again = mount.lookup_path("hello.txt").unwrap();
    let mut buf = vec![0u8; 16];
    let n = mount.read(again, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"world");
}

// ---------------------------------------------------------------------------
// Part 1: blob_size doesn't load full blobs
// ---------------------------------------------------------------------------

/// Wrapping ObjectStore that counts calls to `get_blob`. Used to
/// prove `enumerate`/`attrs` don't pull blob contents through
/// `get_blob` when only the size is needed.
struct CountingStore {
    inner: Box<dyn ObjectStore>,
    get_blob_calls: Arc<AtomicUsize>,
    blob_size_calls: Arc<AtomicUsize>,
}

impl ObjectStore for CountingStore {
    fn get_blob(&self, hash: &ContentHash) -> objects::store::Result<Option<Blob>> {
        self.get_blob_calls.fetch_add(1, Ordering::Relaxed);
        self.inner.get_blob(hash)
    }
    fn put_blob(&self, blob: &Blob) -> objects::store::Result<ContentHash> {
        self.inner.put_blob(blob)
    }
    fn has_blob(&self, hash: &ContentHash) -> objects::store::Result<bool> {
        self.inner.has_blob(hash)
    }
    fn blob_size(&self, hash: &ContentHash) -> objects::store::Result<Option<u64>> {
        self.blob_size_calls.fetch_add(1, Ordering::Relaxed);
        self.inner.blob_size(hash)
    }
    fn get_tree(&self, hash: &ContentHash) -> objects::store::Result<Option<Tree>> {
        self.inner.get_tree(hash)
    }
    fn put_tree(&self, tree: &Tree) -> objects::store::Result<ContentHash> {
        self.inner.put_tree(tree)
    }
    fn has_tree(&self, hash: &ContentHash) -> objects::store::Result<bool> {
        self.inner.has_tree(hash)
    }
    fn get_state(&self, id: &ChangeId) -> objects::store::Result<Option<State>> {
        self.inner.get_state(id)
    }
    fn put_state(&self, state: &State) -> objects::store::Result<()> {
        self.inner.put_state(state)
    }
    fn has_state(&self, id: &ChangeId) -> objects::store::Result<bool> {
        self.inner.has_state(id)
    }
    fn list_states(&self) -> objects::store::Result<Vec<ChangeId>> {
        self.inner.list_states()
    }
    fn get_action(&self, id: &ActionId) -> objects::store::Result<Option<Action>> {
        self.inner.get_action(id)
    }
    fn put_action(&self, action: &mut Action) -> objects::store::Result<ActionId> {
        self.inner.put_action(action)
    }
    fn list_actions(&self) -> objects::store::Result<Vec<ActionId>> {
        self.inner.list_actions()
    }
    fn list_blobs(&self) -> objects::store::Result<Vec<ContentHash>> {
        self.inner.list_blobs()
    }
    fn list_trees(&self) -> objects::store::Result<Vec<ContentHash>> {
        self.inner.list_trees()
    }
}

#[test]
fn enumerate_serves_size_without_loading_blob_bytes() {
    // Build a fixture, then re-open the repo with a counting store
    // wrapped around the FsStore. enumerate() must return sizes
    // without ever calling get_blob — only blob_size.
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    fs::write(temp.path().join("a.txt"), b"first").unwrap();
    fs::write(temp.path().join("b.txt"), b"second-larger-payload").unwrap();
    fs::write(temp.path().join("c.txt"), vec![0u8; 4096]).unwrap();
    repo.snapshot(Some("fixture".into()), None).unwrap();
    drop(repo);

    let get_blob_calls = Arc::new(AtomicUsize::new(0));
    let blob_size_calls = Arc::new(AtomicUsize::new(0));
    let inner: Box<dyn ObjectStore> =
        Box::new(objects::store::FsStore::new(temp.path().join(".heddle")));
    let store = CountingStore {
        inner,
        get_blob_calls: get_blob_calls.clone(),
        blob_size_calls: blob_size_calls.clone(),
    };
    let repo = Repository::open_with_store(temp.path().join(".heddle"), Box::new(store)).unwrap();
    let mount = ContentAddressedMount::new(repo, "main").unwrap();

    let entries = mount.enumerate(NodeId::ROOT).unwrap();
    let names: Vec<_> = entries
        .iter()
        .map(|e| e.name.to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"a.txt".to_string()));
    assert!(names.contains(&"b.txt".to_string()));
    assert!(names.contains(&"c.txt".to_string()));
    let a_size = entries
        .iter()
        .find(|e| e.name == "a.txt")
        .map(|e| e.size)
        .unwrap();
    assert_eq!(a_size, 5);
    let c_size = entries
        .iter()
        .find(|e| e.name == "c.txt")
        .map(|e| e.size)
        .unwrap();
    assert_eq!(c_size, 4096);

    // The killer assertion: enumerate() must not have pulled blob
    // bytes. blob_size() should have been called for each blob entry,
    // get_blob() never.
    assert_eq!(
        get_blob_calls.load(Ordering::Relaxed),
        0,
        "enumerate() pulled blob bytes when only size was needed"
    );
    assert!(
        blob_size_calls.load(Ordering::Relaxed) >= 3,
        "expected blob_size to be called at least once per blob entry"
    );

    // Same expectation for attrs().
    let prior_get_blob = get_blob_calls.load(Ordering::Relaxed);
    let node = mount.lookup_path("c.txt").unwrap();
    let _attrs = mount.attrs(node).unwrap();
    assert_eq!(
        get_blob_calls.load(Ordering::Relaxed),
        prior_get_blob,
        "attrs() pulled blob bytes when only size was needed"
    );
}

// ---------------------------------------------------------------------------
// Part 2: two-tier write model
// ---------------------------------------------------------------------------

/// Build a fresh repo + mount pointing at `main`. The repo is empty
/// (no captured state beyond the seeded empty-tree main).
fn fresh_mount() -> (TempDir, ContentAddressedMount) {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let mount = ContentAddressedMount::new(repo, "main").unwrap();
    (temp, mount)
}

/// Mint a brand-new file path in the mount via lookup, returning a
/// NodeId we can write to. The mount has no `create()` entrypoint
/// yet (FUSE wires that separately); for tests we install a
/// `PendingFile` record directly. This mirrors what the `create`
/// callback will ultimately do.
fn create_pending_file(
    mount: &ContentAddressedMount,
    name: &str,
    mode: objects::object::FileMode,
) -> NodeId {
    use crate::core::test_helpers::install_pending_file;
    install_pending_file(mount, name, mode)
}

#[test]
fn write_then_read_same_file() {
    let (_temp, mount) = fresh_mount();
    let node = create_pending_file(&mount, "draft.txt", objects::object::FileMode::Normal);
    let written = mount.write(node, 0, b"hello mount").unwrap();
    assert_eq!(written, 11);

    let mut buf = vec![0u8; 64];
    let n = mount.read(node, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"hello mount");
}

#[test]
fn flush_promotes_buffer_to_warm_tier() {
    let (_temp, mount) = fresh_mount();
    let node = create_pending_file(&mount, "out.txt", objects::object::FileMode::Normal);
    mount.write(node, 0, b"promote me").unwrap();
    assert_eq!(mount.hot_buffer_count(), 1);
    assert!(mount.warm_keys().is_empty());

    mount.flush(node).unwrap();
    assert_eq!(mount.hot_buffer_count(), 0, "hot buffer should be drained");
    let warm = mount.warm_keys();
    assert_eq!(warm.len(), 1);
    assert_eq!(warm[0], std::path::PathBuf::from("out.txt"));
}

#[test]
fn captured_file_read_after_flush_through_same_node_id_serves_overlay() {
    // Regression: the FUSE shell reuses the NodeId the kernel cached
    // for a captured-tree file across the open → write → close →
    // reopen → read cycle (FUSE's dentry TTL keeps the dentry alive
    // for the cache window, so the kernel never re-issues `lookup`).
    // The core's `read` must therefore consult the pending overlay
    // for a `NodeRecord::File`'s path before falling back to the
    // captured blob — otherwise post-flush reads through the same
    // NodeId silently return the *pre*-write bytes and "write through
    // the mount" looks broken from userspace.
    //
    // The companion `lookup_after_write_serves_new_content` test
    // doesn't trip this because it re-resolves via `lookup_path`
    // after the flush, which refreshes the inode record. Real FUSE
    // dispatchers don't do that re-resolution — the kernel does.
    let (_temp, mount) = open_mount();
    let node = mount.lookup_path("hello.txt").unwrap();

    // Sanity: pre-write content from the captured tree.
    let mut buf = vec![0u8; 64];
    let n = mount.read(node, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"world");

    // Write through the *captured* file's NodeId, flush to warm.
    mount.write(node, 0, b"WORLD").unwrap();
    mount.flush(node).unwrap();

    // Re-read via the same NodeId — no fresh `lookup_path` call.
    let mut buf = vec![0u8; 64];
    let n = mount.read(node, 0, &mut buf).unwrap();
    assert_eq!(
        &buf[..n],
        b"WORLD",
        "captured-file read after flush must serve warm tier, not captured blob"
    );
    let attrs = mount.attrs(node).unwrap();
    assert_eq!(
        attrs.size, 5,
        "captured-file attrs after flush must reflect warm-tier size"
    );
}

#[test]
fn lookup_after_write_serves_new_content() {
    // Write a new file via the pending tier, then look it up by
    // path and read through the resulting NodeId. Should return the
    // bytes we just wrote, not whatever the captured tree said.
    let (_temp, mount) = fresh_mount();
    let node = create_pending_file(&mount, "fresh.md", objects::object::FileMode::Normal);
    mount.write(node, 0, b"# fresh\n").unwrap();

    // Hot-tier read-after-write through lookup.
    let looked_up = mount.lookup_path("fresh.md").unwrap();
    let mut buf = vec![0u8; 64];
    let n = mount.read(looked_up, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"# fresh\n");

    // Promote and re-read — now it's in the warm tier.
    mount.flush(node).unwrap();
    let looked_up_warm = mount.lookup_path("fresh.md").unwrap();
    let n = mount.read(looked_up_warm, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"# fresh\n");
}

#[test]
fn cross_thread_blob_dedup() {
    // The killer demo. Two mounts against two different threads of
    // the same repo write identical content to *different* paths.
    // The pending tier promotes both to CAS via put_blob, which is
    // content-addressed: the same bytes hash to the same blob_oid,
    // so the store ends up with exactly one blob — not two.
    let temp = TempDir::new().unwrap();
    let repo_a = Repository::init_default(temp.path()).unwrap();
    // Add a sibling thread by reusing the seeded `main` head.
    let main_id = repo_a
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .unwrap();
    repo_a
        .refs()
        .set_thread(&ThreadName::new("feature"), &main_id)
        .unwrap();
    drop(repo_a);

    // Open two independent mounts against the same backing store.
    let repo_main = Repository::open(temp.path()).unwrap();
    let mount_main = ContentAddressedMount::new(repo_main, "main").unwrap();
    let repo_feat = Repository::open(temp.path()).unwrap();
    let mount_feat = ContentAddressedMount::new(repo_feat, "feature").unwrap();

    let payload = b"shared module content\n// dedup demo\n";

    let n_main = create_pending_file(&mount_main, "lib.rs", objects::object::FileMode::Normal);
    mount_main.write(n_main, 0, payload).unwrap();
    mount_main.flush(n_main).unwrap();

    let n_feat = create_pending_file(&mount_feat, "module.rs", objects::object::FileMode::Normal);
    mount_feat.write(n_feat, 0, payload).unwrap();
    mount_feat.flush(n_feat).unwrap();

    // Both warm tiers point at the same blob oid — content-addressed
    // dedup falls out for free.
    let oid_a = mount_main.warm_blob("lib.rs").expect("a promoted");
    let oid_b = mount_feat.warm_blob("module.rs").expect("b promoted");
    assert_eq!(
        oid_a, oid_b,
        "identical content must hash to the same blob_oid across threads"
    );

    // Verify only one blob exists in the underlying store with that
    // hash. (list_blobs returns all unique hashes; we just check
    // ours appears once.)
    let repo_check = Repository::open(temp.path()).unwrap();
    let blobs = repo_check.store().list_blobs().unwrap();
    let count = blobs.iter().filter(|h| **h == oid_a).count();
    assert_eq!(
        count, 1,
        "writing the same payload to two threads must yield exactly one blob in the store"
    );
}

#[test]
fn capture_builds_state_and_advances_thread() {
    // Write several files via the pending tier, then capture()
    // them into a real heddle state. Verify:
    //   1. A new state exists in the store.
    //   2. The thread's HEAD points at the new state.
    //   3. The state's tree contains the files we wrote with the
    //      blob hashes we promoted.
    let (_temp, mount) = fresh_mount();
    let n1 = create_pending_file(&mount, "alpha.txt", objects::object::FileMode::Normal);
    mount.write(n1, 0, b"alpha").unwrap();
    let n2 = create_pending_file(&mount, "beta.txt", objects::object::FileMode::Normal);
    mount.write(n2, 0, b"beta!").unwrap();
    mount.flush_all().unwrap();
    let alpha_oid = mount.warm_blob("alpha.txt").unwrap();
    let beta_oid = mount.warm_blob("beta.txt").unwrap();

    let prior_head = mount.current_change_id();
    let new_id = mount.capture(Some("two files".to_string())).unwrap();
    assert_ne!(new_id, prior_head, "capture should advance the thread");

    // Open a fresh repository handle to read post-capture state.
    let new_state = match dig_state(&mount, &new_id) {
        Some(s) => s,
        None => panic!("captured state not found in store"),
    };
    let new_tree = mount
        .repo_handle()
        .store()
        .get_tree(&new_state.tree)
        .unwrap()
        .unwrap();
    let names: Vec<&str> = new_tree.entries().iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"alpha.txt"));
    assert!(names.contains(&"beta.txt"));
    assert_eq!(
        new_tree
            .get("alpha.txt")
            .map(|e| e.hash)
            .expect("alpha entry"),
        alpha_oid
    );
    assert_eq!(
        new_tree
            .get("beta.txt")
            .map(|e| e.hash)
            .expect("beta entry"),
        beta_oid
    );

    // Thread HEAD has advanced.
    let repo_check = mount.repo_handle();
    let head = repo_check
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .unwrap();
    assert_eq!(head, new_id);
}

fn dig_state(mount: &ContentAddressedMount, id: &ChangeId) -> Option<State> {
    mount.repo_handle().store().get_state(id).ok().flatten()
}

// Silence unused-import warnings the compiler can't see through the
// helpers above.
#[allow(dead_code)]
fn _force_unused(e: HeddleError) -> HeddleError {
    e
}

// ---------------------------------------------------------------------------
// Part 3: nested-tree fold-up (Task A)
// ---------------------------------------------------------------------------

/// Walk a path component-by-component through `lookup`, mirroring how
/// FUSE actually descends. Catches regressions in the `Dir` parent
/// path-tracking that `lookup_path` (which uses the registry) might
/// hide.
fn lookup_path_via_components(
    mount: &ContentAddressedMount,
    path: &str,
) -> Option<crate::shell::Entry> {
    let mut node = NodeId::ROOT;
    let mut last = None;
    for comp in std::path::Path::new(path).components() {
        let std::path::Component::Normal(name) = comp else {
            continue;
        };
        let entry = mount.lookup(node, name).ok().flatten()?;
        node = entry.node;
        last = Some(entry);
    }
    last
}

#[test]
fn capture_nested_new_file_under_existing_dir() {
    // The fixture has `nested/inner.txt`. Write a brand-new file at
    // `nested/extra.rs` via the mount; capture must produce a new
    // tree where `nested/` contains both the original entries AND
    // the new file with the right blob.
    let (_temp, mount) = open_mount();
    let node = create_pending_file(&mount, "nested/extra.rs", objects::object::FileMode::Normal);
    mount.write(node, 0, b"// fresh\n").unwrap();
    mount.flush_all().unwrap();
    let extra_blob = mount.warm_blob("nested/extra.rs").unwrap();

    let new_id = mount.capture(Some("nested write".into())).unwrap();

    // Resolve `nested/extra.rs` in the captured tree.
    let store = mount.repo_handle().store();
    let state = store.get_state(&new_id).unwrap().unwrap();
    let root_tree = store.get_tree(&state.tree).unwrap().unwrap();
    let nested_entry = root_tree.get("nested").expect("nested dir");
    assert!(nested_entry.is_tree());
    let nested = store.get_tree(&nested_entry.hash).unwrap().unwrap();
    let names: Vec<&str> = nested.entries().iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"inner.txt"));
    assert!(names.contains(&"note.md"));
    assert!(names.contains(&"extra.rs"));
    assert_eq!(nested.get("extra.rs").unwrap().hash, extra_blob);
}

#[test]
fn capture_creates_new_intermediate_dirs() {
    // `newdir/` doesn't exist in the captured tree at all. A write
    // to `newdir/bar.rs` must produce a new state whose root has a
    // `newdir` subtree containing `bar.rs`.
    let (_temp, mount) = open_mount();
    let node = create_pending_file(&mount, "newdir/bar.rs", objects::object::FileMode::Normal);
    mount.write(node, 0, b"newcontent").unwrap();
    mount.flush_all().unwrap();

    let new_id = mount.capture(Some("new dir".into())).unwrap();

    let store = mount.repo_handle().store();
    let state = store.get_state(&new_id).unwrap().unwrap();
    let root_tree = store.get_tree(&state.tree).unwrap().unwrap();
    let newdir_entry = root_tree.get("newdir").expect("newdir created");
    assert!(newdir_entry.is_tree());
    let newdir = store.get_tree(&newdir_entry.hash).unwrap().unwrap();
    assert_eq!(newdir.entries().len(), 1);
    assert_eq!(newdir.entries()[0].name, "bar.rs");
}

#[test]
fn capture_handles_multiple_files_at_multiple_depths() {
    let (_temp, mount) = open_mount();
    // Touch four paths at four depths.
    let paths = [
        "top.txt",
        "nested/extra.rs",
        "deep/again/level3.rs",
        "deep/sibling.rs",
    ];
    for p in paths {
        let node = create_pending_file(&mount, p, objects::object::FileMode::Normal);
        mount.write(node, 0, p.as_bytes()).unwrap();
    }
    mount.flush_all().unwrap();

    let new_id = mount.capture(Some("many paths".into())).unwrap();
    let store = mount.repo_handle().store();
    let state = store.get_state(&new_id).unwrap().unwrap();
    let root_tree = store.get_tree(&state.tree).unwrap().unwrap();

    // Every path should resolve.
    for p in paths {
        let entry = root_tree.get_path(std::path::Path::new(p));
        let resolved = if entry.is_some() {
            entry.cloned()
        } else {
            // Path may live deeper; walk segments.
            let mut current = root_tree.clone();
            let mut last = None;
            for comp in std::path::Path::new(p).components() {
                let std::path::Component::Normal(name) = comp else {
                    continue;
                };
                let name = name.to_str().unwrap();
                match current.get(name).cloned() {
                    Some(e) if e.is_tree() => {
                        current = store.get_tree(&e.hash).unwrap().unwrap();
                        last = Some(e);
                    }
                    Some(e) => {
                        last = Some(e);
                        break;
                    }
                    None => {
                        last = None;
                        break;
                    }
                }
            }
            last
        };
        assert!(resolved.is_some(), "path {} missing in captured tree", p);
    }
}

#[test]
fn lookup_serves_implicit_pending_dir_before_capture() {
    // Before capture, writing `newdir/foo.rs` should make `newdir`
    // resolvable as an *implicit* directory through component-wise
    // lookup, and the file readable through it.
    let (_temp, mount) = fresh_mount();
    let node = create_pending_file(&mount, "newdir/foo.rs", objects::object::FileMode::Normal);
    mount.write(node, 0, b"hello").unwrap();

    let dir_entry = lookup_path_via_components(&mount, "newdir")
        .expect("newdir resolves as implicit pending dir");
    assert_eq!(dir_entry.kind, NodeKind::Directory);

    let file_entry = lookup_path_via_components(&mount, "newdir/foo.rs")
        .expect("newdir/foo.rs resolves through implicit dir");
    let mut buf = vec![0u8; 16];
    let n = mount.read(file_entry.node, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"hello");
}

#[test]
fn capture_unlink_prunes_empty_parent_trees() {
    // Build a fresh mount, write `dir/only.rs`, capture, then mount
    // again and unlink it. The next capture must produce a tree
    // with `dir/` removed (empty parent prune).
    let (_temp, mount) = fresh_mount();
    let node = create_pending_file(&mount, "dir/only.rs", objects::object::FileMode::Normal);
    mount.write(node, 0, b"x").unwrap();
    mount.flush_all().unwrap();
    let _first = mount.capture(Some("plant".into())).unwrap();

    // Now unlink and re-capture.
    mount.unlink_path("dir/only.rs").unwrap();
    let second = mount.capture(Some("delete".into())).unwrap();

    let store = mount.repo_handle().store();
    let state = store.get_state(&second).unwrap().unwrap();
    let root_tree = store.get_tree(&state.tree).unwrap().unwrap();
    assert!(
        root_tree.get("dir").is_none(),
        "empty `dir/` should have been pruned, found: {:?}",
        root_tree
            .entries()
            .iter()
            .map(|e| &e.name)
            .collect::<Vec<_>>()
    );
}

#[test]
fn capture_unlink_drops_only_named_path() {
    // Two files in a dir; deleting one keeps the other.
    let (_temp, mount) = fresh_mount();
    for p in ["dir/keep.rs", "dir/drop.rs"] {
        let node = create_pending_file(&mount, p, objects::object::FileMode::Normal);
        mount.write(node, 0, p.as_bytes()).unwrap();
    }
    mount.flush_all().unwrap();
    let _first = mount.capture(Some("plant".into())).unwrap();

    mount.unlink_path("dir/drop.rs").unwrap();
    let second = mount.capture(Some("delete".into())).unwrap();
    let store = mount.repo_handle().store();
    let state = store.get_state(&second).unwrap().unwrap();
    let root_tree = store.get_tree(&state.tree).unwrap().unwrap();
    let dir_entry = root_tree.get("dir").expect("dir survives");
    let dir = store.get_tree(&dir_entry.hash).unwrap().unwrap();
    let names: Vec<&str> = dir.entries().iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["keep.rs"]);
}

// ---------------------------------------------------------------------------
// Part 4: oplog + thread metadata wiring (Task B)
// ---------------------------------------------------------------------------

#[test]
fn capture_records_oplog_entry() {
    // After a mount-side capture, the oplog should hold a `Snapshot`
    // entry pointing at the new state. Mirrors the CLI capture path.
    use oplog::OpRecord;
    let (_temp, mount) = fresh_mount();
    let prior_count = mount
        .repo_handle()
        .oplog()
        .recent(1024)
        .map(|v| v.len())
        .unwrap_or(0);

    let node = create_pending_file(&mount, "x.txt", objects::object::FileMode::Normal);
    mount.write(node, 0, b"y").unwrap();
    mount.flush_all().unwrap();
    let new_id = mount.capture(Some("oplog".into())).unwrap();

    let entries = mount.repo_handle().oplog().recent(1024).unwrap();
    assert!(entries.len() > prior_count, "oplog entry count grew");
    let saw_snapshot = entries.iter().any(|entry| {
        matches!(&entry.operation, OpRecord::Snapshot { new_state, .. } if *new_state == new_id)
    });
    assert!(
        saw_snapshot,
        "expected a Snapshot oplog entry pointing at the new state {:?}",
        new_id
    );
}

#[test]
fn capture_refreshes_thread_metadata_when_thread_record_exists() {
    // Build a fresh repo + mount with a `Thread` record whose
    // `execution_path` matches the repo root. After capture, the
    // record must reflect the new changed_paths and current_state.
    use chrono::Utc;
    use repo::{Thread, ThreadManager, ThreadMode, ThreadState};

    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let base_state_id = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .unwrap();
    let base_root = repo
        .store()
        .get_state(&base_state_id)
        .unwrap()
        .unwrap()
        .tree
        .to_hex();
    let manager = ThreadManager::new(repo.heddle_dir());
    let thread = Thread {
        id: "main".to_string(),
        thread: "main".to_string(),
        target_thread: None,
        parent_thread: None,
        mode: ThreadMode::Virtualized,
        state: ThreadState::Active,
        base_state: base_state_id.short(),
        base_root,
        current_state: None,
        merged_state: None,
        task: None,
        execution_path: temp.path().to_path_buf(),
        materialized_path: None,
        changed_paths: Vec::new(),
        impact_categories: Vec::new(),
        heavy_impact_paths: Vec::new(),
        promotion_suggested: false,
        freshness: repo::ThreadFreshness::Unknown,
        verification_summary: Default::default(),
        confidence_summary: Default::default(),
        integration_policy_result: Default::default(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        ephemeral: None,
        // Defaults for fields added after this test was written. `auto`
        // mirrors `ThreadRecord::auto` (false = human-authored); the
        // shared cargo target dir is None to mean "per-checkout target/".
        auto: false,
        shared_target_dir: None,
    };
    manager.save(&thread).unwrap();

    let mount = ContentAddressedMount::new(repo, "main").unwrap();
    let node = create_pending_file(
        &mount,
        "src/calculator.rs",
        objects::object::FileMode::Normal,
    );
    mount.write(node, 0, b"// fresh\n").unwrap();
    mount.flush_all().unwrap();
    let new_id = mount.capture(Some("metadata refresh".into())).unwrap();

    let manager = ThreadManager::new(mount.repo_handle().heddle_dir());
    let updated = manager.load("main").unwrap().expect("thread row");
    assert!(
        updated
            .changed_paths
            .iter()
            .any(|p| p == "src/calculator.rs"),
        "changed_paths={:?}",
        updated.changed_paths
    );
    assert_eq!(updated.current_state, Some(new_id.short()));
}

#[test]
fn capture_with_explicit_attribution_lands_on_state() {
    use objects::object::{Agent, Attribution, Principal};

    let (_temp, mount) = fresh_mount();
    let node = create_pending_file(&mount, "x.txt", objects::object::FileMode::Normal);
    mount.write(node, 0, b"y").unwrap();
    mount.flush_all().unwrap();

    let attribution = Attribution::with_agent(
        Principal::new("Test User", "test@example.com"),
        Agent::new("anthropic", "claude-3"),
    );
    let new_id = mount
        .capture_with_attribution(Some("attrib".into()), attribution.clone())
        .unwrap();

    let state = mount
        .repo_handle()
        .store()
        .get_state(&new_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        state.attribution.agent.as_ref().map(|a| a.provider.clone()),
        Some("anthropic".to_string()),
    );
    assert_eq!(
        state.attribution.agent.as_ref().map(|a| a.model.clone()),
        Some("claude-3".to_string()),
    );
}

// ---------------------------------------------------------------------------
// Part 5: clock-driven safety-sweep (Task C)
// ---------------------------------------------------------------------------

#[test]
fn clock_sweep_promotes_idle_buffers() {
    use std::time::Duration;
    let (_temp, repo) = fixture();
    let mount = ContentAddressedMount::new(repo, "main")
        .unwrap()
        .with_promotion_policy(crate::core::PromotionPolicy {
            idle_after: Duration::from_millis(50),
            sweep_interval: Some(Duration::from_millis(80)),
        });

    let node = create_pending_file(&mount, "draft.txt", objects::object::FileMode::Normal);
    mount.write(node, 0, b"sleeping").unwrap();
    assert_eq!(mount.hot_buffer_count(), 1);
    assert!(mount.warm_keys().is_empty());

    // Wait long enough for several sweep iterations to fire after
    // the idle window expires.
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        mount.hot_buffer_count(),
        0,
        "clock sweep should have promoted the idle buffer"
    );
    assert_eq!(mount.warm_keys().len(), 1);
}

#[test]
fn no_sweep_interval_disables_clock_promotion() {
    use std::time::Duration;
    let (_temp, repo) = fixture();
    let mount = ContentAddressedMount::new(repo, "main")
        .unwrap()
        .with_promotion_policy(crate::core::PromotionPolicy {
            idle_after: Duration::from_millis(50),
            sweep_interval: None,
        });

    let node = create_pending_file(&mount, "draft.txt", objects::object::FileMode::Normal);
    mount.write(node, 0, b"sleeping").unwrap();
    std::thread::sleep(Duration::from_millis(250));
    // Without a sweep interval AND without any other mutating call,
    // the hot buffer should still be in the hot tier. (The
    // event-driven sweep only fires on writes; we did exactly one,
    // and that was at t=0 before the idle window even started.)
    assert_eq!(mount.hot_buffer_count(), 1);
    assert!(mount.warm_keys().is_empty());
}

#[test]
fn drop_joins_sweep_thread_cleanly() {
    // Construct a mount with a fast sweep, write a file, then drop
    // the mount. The Drop impl must signal-and-join cleanly without
    // deadlocking. We bound the test with a separate thread + join
    // timeout so a regression doesn't hang CI forever.
    use std::{sync::mpsc::channel, time::Duration};

    let (tx, rx) = channel();
    let join = std::thread::spawn(move || {
        let (_temp, repo) = fixture();
        let mount = ContentAddressedMount::new(repo, "main")
            .unwrap()
            .with_promotion_policy(crate::core::PromotionPolicy {
                idle_after: Duration::from_millis(20),
                sweep_interval: Some(Duration::from_millis(30)),
            });
        let node = create_pending_file(&mount, "x.txt", objects::object::FileMode::Normal);
        mount.write(node, 0, b"k").unwrap();
        std::thread::sleep(Duration::from_millis(80));
        drop(mount);
        let _ = tx.send(());
    });
    let result = rx.recv_timeout(Duration::from_secs(5));
    let join_result = join.join();
    assert!(result.is_ok(), "drop did not complete within 5s");
    assert!(join_result.is_ok(), "test thread panicked");
}

// ---------------------------------------------------------------------------
// Part 6: comprehensive coverage (Task D)
// ---------------------------------------------------------------------------

// D3. Crash recovery test.
#[test]
fn crash_recovery_warm_durable_hot_lost() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    {
        let mount = ContentAddressedMount::new(repo, "main")
            .unwrap()
            .with_promotion_policy(crate::core::PromotionPolicy {
                idle_after: std::time::Duration::from_secs(3600),
                sweep_interval: None,
            });
        let n1 = create_pending_file(&mount, "durable.txt", objects::object::FileMode::Normal);
        mount.write(n1, 0, b"durable").unwrap();
        mount.flush(n1).unwrap(); // promote to warm tier
        let n2 = create_pending_file(&mount, "transient.txt", objects::object::FileMode::Normal);
        mount.write(n2, 0, b"gone").unwrap();
        // No flush. Drop simulates a crash.
        // We can verify the durable blob exists in the store.
        let durable_blob = mount.warm_blob("durable.txt").unwrap();
        let blobs_before = mount.repo_handle().store().list_blobs().unwrap();
        assert!(blobs_before.contains(&durable_blob));
    }
    // Re-open the repo and a new mount on the same backing store.
    let repo = Repository::open(temp.path()).unwrap();
    let mount = ContentAddressedMount::new(repo, "main").unwrap();
    // Hot tier was lost — `transient.txt` doesn't exist.
    let lookup = mount
        .lookup(NodeId::ROOT, OsStr::new("transient.txt"))
        .unwrap();
    assert!(
        lookup.is_none(),
        "hot-tier-only file should be gone after crash"
    );
    // Warm-tier blob is durable in the store, but it was never
    // captured into a state (no `capture()` was called), so the
    // mount's *tree* doesn't surface it either. The durability
    // boundary is the blob, not the tree.
    let durable_lookup = mount
        .lookup(NodeId::ROOT, OsStr::new("durable.txt"))
        .unwrap();
    assert!(
        durable_lookup.is_none(),
        "warm-tier-only file (no capture) is not in the captured tree"
    );
}

// D4. Cross-thread blob dedup at scale.
#[test]
fn cross_thread_blob_dedup_at_scale() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let main_id = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .unwrap();
    // Make 9 sibling threads.
    for i in 0..9 {
        repo.refs()
            .set_thread(&ThreadName::new(format!("feat-{i}")), &main_id)
            .unwrap();
    }
    drop(repo);

    let thread_names: Vec<String> = std::iter::once("main".to_string())
        .chain((0..9).map(|i| format!("feat-{i}")))
        .collect();
    // 10 files per thread; index%2==0 are shared-content, the rest
    // are unique.
    let shared_count = 5; // shared 0..4 across all threads
    for name in &thread_names {
        let r = Repository::open(temp.path()).unwrap();
        let mount = ContentAddressedMount::new(r, name).unwrap();
        for i in 0..10 {
            let path = format!("file{i}.txt");
            let node = create_pending_file(&mount, &path, objects::object::FileMode::Normal);
            let bytes = if i < shared_count {
                format!("shared-content-{i}\n").into_bytes()
            } else {
                format!("unique-{name}-{i}\n").into_bytes()
            };
            mount.write(node, 0, &bytes).unwrap();
        }
        mount.flush_all().unwrap();
    }

    // Now check the blob set: shared_count distinct blobs from shared
    // files, plus 5 unique * 10 threads = 50 unique-content blobs.
    let repo = Repository::open(temp.path()).unwrap();
    let blobs: std::collections::HashSet<_> =
        repo.store().list_blobs().unwrap().into_iter().collect();
    // Empty blob from initial seed may also be present, so we check
    // we have at *least* shared_count + 50 distinct blobs and at
    // most a small constant overhead beyond.
    let expected_unique = shared_count + 5 * 10;
    assert!(
        blobs.len() >= expected_unique && blobs.len() <= expected_unique + 4,
        "expected ~{expected_unique} distinct blobs, got {}",
        blobs.len()
    );
}

// ---------------------------------------------------------------------------
// Part 5: write-side overlay ops — create / mkdir / unlink / rmdir / rename /
// setattr / symlink. Each op exercises the PlatformShell trait method on a
// real ContentAddressedMount, so the test catches both the core implementation
// and the trait dispatch (it's the same path FUSE / FSKit / ProjFS callbacks
// take). Issue: heddle#180 — unblocks `open(O_CREAT)` and friends on the
// Linux FUSE shell.
// ---------------------------------------------------------------------------
mod write_ops {
    use std::path::Path;

    use objects::object::FileMode;

    use super::*;
    use crate::shell::AttrUpdate;

    /// `create_file` mints a fresh PendingFile under root, visible to
    /// subsequent `lookup` / `read` calls. The first `write` against
    /// the returned NodeId seeds an empty buffer; the byte stream
    /// flows through the existing two-tier write model.
    #[test]
    fn create_file_in_root_then_write_and_read_back() {
        let (_temp, mount) = open_mount();
        let entry = mount
            .create_file(
                NodeId::ROOT,
                OsStr::new("Cargo.lock"),
                FileMode::Normal,
                false,
            )
            .expect("create_file");
        assert_eq!(entry.kind, NodeKind::File);
        assert_eq!(entry.name, "Cargo.lock");

        // Lookup must now resolve the same path.
        let looked_up = mount
            .lookup(NodeId::ROOT, OsStr::new("Cargo.lock"))
            .expect("lookup ok")
            .expect("lookup hit");
        assert_eq!(looked_up.node, entry.node);

        // Write + read-back through the freshly minted NodeId.
        mount.write(entry.node, 0, b"[package]\n").expect("write");
        let mut buf = vec![0u8; 32];
        let n = mount.read(entry.node, 0, &mut buf).expect("read");
        assert_eq!(&buf[..n], b"[package]\n");
    }

    /// `create_file` with `exclusive=true` (`O_CREAT|O_EXCL`) against
    /// an already-existing captured path must fail with
    /// `AlreadyExists` (errno `EEXIST`).
    #[test]
    fn create_file_exclusive_against_existing_returns_eexist() {
        let (_temp, mount) = open_mount();
        let err = mount
            .create_file(
                NodeId::ROOT,
                OsStr::new("hello.txt"),
                FileMode::Normal,
                true,
            )
            .expect_err("exclusive create on existing must fail");
        assert!(matches!(err, MountError::AlreadyExists(_)), "got {err:?}");
        assert_eq!(err.to_errno(), libc::EEXIST);
    }

    /// `create_file` with `exclusive=false` (`O_CREAT` without
    /// `O_EXCL`) against an existing captured path returns the
    /// existing entry — that's the POSIX `open(O_CREAT)` shape, and
    /// what cargo / rustc rely on when re-opening an artifact for
    /// rewrite.
    #[test]
    fn create_file_non_exclusive_returns_existing_entry() {
        let (_temp, mount) = open_mount();
        let entry = mount
            .create_file(
                NodeId::ROOT,
                OsStr::new("hello.txt"),
                FileMode::Normal,
                false,
            )
            .expect("non-exclusive create on existing returns entry");
        assert_eq!(entry.name, "hello.txt");
        let captured = mount
            .lookup(NodeId::ROOT, OsStr::new("hello.txt"))
            .unwrap()
            .unwrap();
        assert_eq!(captured.node, entry.node);
    }

    /// Names containing `/` or `\0`, or the reserved `.` / `..`
    /// pseudo-entries, must be rejected at the create boundary with
    /// `EINVAL` — not silently shoved into the overlay.
    #[test]
    fn create_file_rejects_invalid_names() {
        let (_temp, mount) = open_mount();
        for bad in ["", ".", "..", "a/b", "with\0nul"] {
            let err = mount
                .create_file(NodeId::ROOT, OsStr::new(bad), FileMode::Normal, false)
                .expect_err(&format!("name {bad:?} must be rejected"));
            assert!(
                matches!(err, MountError::InvalidArgument(_)),
                "{bad}: {err:?}"
            );
            assert_eq!(err.to_errno(), libc::EINVAL);
        }
    }

    /// `make_dir` creates an empty pending directory under root that
    /// shows up in lookup + enumerate immediately. Subsequent
    /// `create_file` calls under it must work.
    #[test]
    fn make_dir_creates_empty_visible_dir() {
        let (_temp, mount) = open_mount();
        let dir_entry = mount
            .make_dir(NodeId::ROOT, OsStr::new("target"))
            .expect("make_dir");
        assert_eq!(dir_entry.kind, NodeKind::Directory);

        // Visible via lookup.
        let looked = mount
            .lookup(NodeId::ROOT, OsStr::new("target"))
            .unwrap()
            .unwrap();
        assert_eq!(looked.node, dir_entry.node);

        // Root enumerate must include the new directory.
        let root_entries = mount.enumerate(NodeId::ROOT).unwrap();
        assert!(
            root_entries
                .iter()
                .any(|e| e.name == "target" && e.kind == NodeKind::Directory),
            "root enumerate did not include the new dir: {root_entries:?}"
        );

        // Create a file inside it; visible via lookup under the dir.
        let file = mount
            .create_file(
                dir_entry.node,
                OsStr::new("out.bin"),
                FileMode::Normal,
                false,
            )
            .expect("create_file under new dir");
        let from_lookup = mount
            .lookup(dir_entry.node, OsStr::new("out.bin"))
            .unwrap()
            .unwrap();
        assert_eq!(from_lookup.node, file.node);
    }

    /// `make_dir` against an existing path (captured or pending) is
    /// `EEXIST`. POSIX `mkdir(2)` shape.
    #[test]
    fn make_dir_existing_returns_eexist() {
        let (_temp, mount) = open_mount();
        // `nested/` already exists in the fixture's captured tree.
        let err = mount
            .make_dir(NodeId::ROOT, OsStr::new("nested"))
            .expect_err("mkdir on existing must fail");
        assert_eq!(err.to_errno(), libc::EEXIST);
    }

    /// `unlink_entry` against a captured file tombstones it: post-
    /// unlink lookup returns `None`, enumerate skips it, and a
    /// subsequent `create_file` (POSIX `unlink+open(O_CREAT)`) mints
    /// a fresh empty inode at the same path.
    #[test]
    fn unlink_entry_removes_captured_file_and_allows_recreate() {
        let (_temp, mount) = open_mount();
        mount
            .unlink_entry(NodeId::ROOT, OsStr::new("hello.txt"))
            .expect("unlink");
        assert!(
            mount
                .lookup(NodeId::ROOT, OsStr::new("hello.txt"))
                .unwrap()
                .is_none(),
            "post-unlink lookup must return None"
        );
        let entries = mount.enumerate(NodeId::ROOT).unwrap();
        assert!(
            !entries.iter().any(|e| e.name == "hello.txt"),
            "enumerate still surfaces unlinked file"
        );

        // Recreate.
        let recreated = mount
            .create_file(
                NodeId::ROOT,
                OsStr::new("hello.txt"),
                FileMode::Normal,
                false,
            )
            .expect("recreate after unlink");
        mount.write(recreated.node, 0, b"REBORN").unwrap();
        let mut buf = vec![0u8; 16];
        let n = mount.read(recreated.node, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"REBORN");
    }

    /// `unlink_entry` on a directory is `EISDIR` (POSIX `unlink(2)`).
    #[test]
    fn unlink_entry_on_directory_returns_eisdir() {
        let (_temp, mount) = open_mount();
        let err = mount
            .unlink_entry(NodeId::ROOT, OsStr::new("nested"))
            .expect_err("unlink on dir must fail");
        assert_eq!(err.to_errno(), libc::EISDIR);
    }

    /// `unlink_entry` on a name that doesn't exist is `ENOENT`.
    #[test]
    fn unlink_entry_missing_returns_enoent() {
        let (_temp, mount) = open_mount();
        let err = mount
            .unlink_entry(NodeId::ROOT, OsStr::new("nonexistent"))
            .expect_err("unlink missing must fail");
        assert_eq!(err.to_errno(), libc::ENOENT);
    }

    /// `rmdir_entry` removes an empty pending directory. Subsequent
    /// lookup returns `None`.
    #[test]
    fn rmdir_entry_removes_empty_pending_dir() {
        let (_temp, mount) = open_mount();
        let dir = mount.make_dir(NodeId::ROOT, OsStr::new("scratch")).unwrap();
        let _ = dir; // keep alive
        mount
            .rmdir_entry(NodeId::ROOT, OsStr::new("scratch"))
            .expect("rmdir");
        assert!(mount
            .lookup(NodeId::ROOT, OsStr::new("scratch"))
            .unwrap()
            .is_none());
    }

    /// `rmdir_entry` on a directory that has any visible child (pending
    /// or captured) must fail with `ENOTEMPTY`.
    #[test]
    fn rmdir_entry_non_empty_returns_enotempty() {
        let (_temp, mount) = open_mount();
        // The fixture's `nested/` has captured children.
        let err = mount
            .rmdir_entry(NodeId::ROOT, OsStr::new("nested"))
            .expect_err("rmdir on non-empty must fail");
        assert_eq!(err.to_errno(), libc::ENOTEMPTY);
    }

    /// `rmdir_entry` on a regular file is `ENOTDIR`.
    #[test]
    fn rmdir_entry_on_file_returns_enotdir() {
        let (_temp, mount) = open_mount();
        let err = mount
            .rmdir_entry(NodeId::ROOT, OsStr::new("hello.txt"))
            .expect_err("rmdir on file must fail");
        assert_eq!(err.to_errno(), libc::ENOTDIR);
    }

    /// File rename within the same directory: source disappears,
    /// destination resolves to the renamed file with the same bytes.
    /// This is the cargo / git path: write `foo.tmp` then rename to
    /// `foo`.
    #[test]
    fn rename_entry_file_same_dir() {
        let (_temp, mount) = open_mount();
        let src = mount
            .create_file(
                NodeId::ROOT,
                OsStr::new("Cargo.lock.tmp"),
                FileMode::Normal,
                false,
            )
            .unwrap();
        mount.write(src.node, 0, b"[atomic]\n").unwrap();
        mount.flush(src.node).unwrap();

        mount
            .rename_entry(
                NodeId::ROOT,
                OsStr::new("Cargo.lock.tmp"),
                NodeId::ROOT,
                OsStr::new("Cargo.lock"),
            )
            .expect("rename");

        assert!(
            mount
                .lookup(NodeId::ROOT, OsStr::new("Cargo.lock.tmp"))
                .unwrap()
                .is_none(),
            "source path must be gone after rename"
        );
        let dst = mount
            .lookup(NodeId::ROOT, OsStr::new("Cargo.lock"))
            .unwrap()
            .expect("dst resolves");
        let mut buf = vec![0u8; 16];
        let n = mount.read(dst.node, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"[atomic]\n");
    }

    /// Rename across directories: src in `nested/`, dst in root.
    #[test]
    fn rename_entry_cross_dir() {
        let (_temp, mount) = open_mount();
        // Source lives in the captured tree at `nested/inner.txt`.
        let nested = mount
            .lookup(NodeId::ROOT, OsStr::new("nested"))
            .unwrap()
            .unwrap();
        mount
            .rename_entry(
                nested.node,
                OsStr::new("inner.txt"),
                NodeId::ROOT,
                OsStr::new("moved.txt"),
            )
            .expect("cross-dir rename");

        assert!(
            mount
                .lookup(nested.node, OsStr::new("inner.txt"))
                .unwrap()
                .is_none(),
            "source path must be gone after rename"
        );
        let dst = mount
            .lookup(NodeId::ROOT, OsStr::new("moved.txt"))
            .unwrap()
            .expect("dst resolves");
        let mut buf = vec![0u8; 32];
        let n = mount.read(dst.node, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"deep");
    }

    /// Rename onto an existing file of the same kind: POSIX allows it
    /// (atomic replace). The destination's prior content is gone.
    #[test]
    fn rename_entry_replaces_existing_file() {
        let (_temp, mount) = open_mount();
        let src = mount
            .create_file(NodeId::ROOT, OsStr::new("draft"), FileMode::Normal, false)
            .unwrap();
        mount.write(src.node, 0, b"draft body").unwrap();
        mount.flush(src.node).unwrap();
        // hello.txt exists in the fixture; rename overwrites it.
        mount
            .rename_entry(
                NodeId::ROOT,
                OsStr::new("draft"),
                NodeId::ROOT,
                OsStr::new("hello.txt"),
            )
            .expect("rename-over");
        let dst = mount
            .lookup(NodeId::ROOT, OsStr::new("hello.txt"))
            .unwrap()
            .unwrap();
        let mut buf = vec![0u8; 32];
        let n = mount.read(dst.node, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"draft body");
    }

    /// After `unlink_entry` the path→inode mapping must be retired so
    /// a subsequent `create_file` at the same name mints a *fresh*
    /// inode. Otherwise a still-open handle to the unlinked file would
    /// silently start resolving to the freshly created replacement —
    /// breaks unlink-then-recreate isolation (POSIX open-unlinked temp
    /// files).
    #[test]
    fn unlink_then_recreate_mints_fresh_inode() {
        let (_temp, mount) = open_mount();
        let original = mount
            .lookup(NodeId::ROOT, OsStr::new("hello.txt"))
            .unwrap()
            .expect("captured hello.txt");
        mount
            .unlink_entry(NodeId::ROOT, OsStr::new("hello.txt"))
            .expect("unlink");
        let recreated = mount
            .create_file(
                NodeId::ROOT,
                OsStr::new("hello.txt"),
                FileMode::Normal,
                false,
            )
            .expect("recreate");
        assert_ne!(
            original.node, recreated.node,
            "recreated inode must be distinct from the unlinked one"
        );
    }

    /// POSIX `unlink(2)` semantics: if a file is open when it's
    /// unlinked, the kernel keeps the inode alive behind the open
    /// fd, but the *directory entry* is gone — `lookup` returns
    /// `ENOENT` and `readdir` skips the name. A subsequent write
    /// through the open fd updates the orphaned inode's data, but
    /// it must NOT republish the name. Tools that depend on this:
    /// `mkstemp` + `unlink` for private scratch space, sqlite's WAL
    /// shadow files, cargo's atomic-replace pattern. Without the
    /// guard, the unlinked pathname unexpectedly reappears once a
    /// late write hits the open fd.
    #[test]
    fn write_to_unlinked_open_inode_does_not_resurrect_path() {
        let (_temp, mount) = open_mount();
        // `fd = open("temp", O_CREAT|O_RDWR)` — fresh pending file.
        let entry = mount
            .create_file(NodeId::ROOT, OsStr::new("temp"), FileMode::Normal, false)
            .expect("create");
        // The `O_CREAT|O_RDWR` open above bumps `open_count` to 1 —
        // without this the witness-gated unlink (heddle#209) sees the
        // node as `Released` (no entry) and skips the orphan
        // transition, breaking the open-unlinked POSIX flow this test
        // exercises.
        mount.on_open(entry.node).expect("on_open");
        mount.write(entry.node, 0, b"v1").expect("first write");
        // `unlink("temp")` while the handle is still in use.
        mount
            .unlink_entry(NodeId::ROOT, OsStr::new("temp"))
            .expect("unlink");
        assert!(
            mount
                .lookup(NodeId::ROOT, OsStr::new("temp"))
                .unwrap()
                .is_none(),
            "post-unlink lookup must return None"
        );

        // Write through the original handle. POSIX says this is
        // legal — the inode survives behind the fd — but the
        // pathname must not come back.
        mount
            .write(entry.node, 0, b"v2-after-unlink")
            .expect("write through unlinked-open fd");

        // The data is accessible through the open handle.
        let mut buf = vec![0u8; 64];
        let n = mount
            .read(entry.node, 0, &mut buf)
            .expect("read via unlinked-open fd");
        assert_eq!(&buf[..n], b"v2-after-unlink");

        // The decisive check: the path must still be gone.
        assert!(
            mount
                .lookup(NodeId::ROOT, OsStr::new("temp"))
                .unwrap()
                .is_none(),
            "write after unlink must not resurrect the path"
        );
        let entries = mount.enumerate(NodeId::ROOT).unwrap();
        assert!(
            !entries.iter().any(|e| e.name == "temp"),
            "enumerate must not surface the unlinked path: {entries:?}"
        );
        // Flushing the orphan must not promote it to the warm tier
        // (a subsequent capture would resurrect the path in the
        // captured tree otherwise). Drive the flush explicitly so
        // the test pins the contract; orphaned buffers must drop.
        mount.flush(entry.node).expect("flush orphan");
        assert!(
            mount
                .lookup(NodeId::ROOT, OsStr::new("temp"))
                .unwrap()
                .is_none(),
            "post-flush lookup must still be gone (orphan must not warm-promote)"
        );
    }

    /// Companion to the orphan-write test: after an unlink, if a
    /// fresh `create_file` mints a new inode at the same name,
    /// writes through the *new* fd must surface normally. The
    /// orphan-write fix must not regress the unlink-then-recreate
    /// path the kernel actually emits for `open(O_CREAT)`.
    #[test]
    fn write_to_recreated_inode_after_unlink_still_publishes_path() {
        let (_temp, mount) = open_mount();
        let original = mount
            .create_file(NodeId::ROOT, OsStr::new("temp"), FileMode::Normal, false)
            .expect("create v1");
        mount.write(original.node, 0, b"v1").expect("write v1");
        mount
            .unlink_entry(NodeId::ROOT, OsStr::new("temp"))
            .expect("unlink");

        let recreated = mount
            .create_file(NodeId::ROOT, OsStr::new("temp"), FileMode::Normal, false)
            .expect("recreate");
        assert_ne!(original.node, recreated.node);
        mount
            .write(recreated.node, 0, b"v2-fresh")
            .expect("write v2");

        // The new inode is the one that owns the path now.
        let hit = mount
            .lookup(NodeId::ROOT, OsStr::new("temp"))
            .unwrap()
            .expect("recreated path must resolve");
        assert_eq!(hit.node, recreated.node);
        let mut buf = vec![0u8; 16];
        let n = mount.read(recreated.node, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"v2-fresh");
    }

    /// Rename-over must keep the replaced destination's inode record
    /// resolvable — only the path→inode link is detached. Without this
    /// any FD still holding the dest inode surfaces as ESTALE on the
    /// next callback. POSIX requires open handles to the replaced file
    /// to remain valid.
    #[test]
    fn rename_over_preserves_replaced_destination_inode() {
        let (_temp, mount) = open_mount();
        let dest_orig = mount
            .lookup(NodeId::ROOT, OsStr::new("hello.txt"))
            .unwrap()
            .expect("captured hello.txt");
        let src = mount
            .create_file(NodeId::ROOT, OsStr::new("draft"), FileMode::Normal, false)
            .unwrap();
        mount.write(src.node, 0, b"replacement").unwrap();
        mount.flush(src.node).unwrap();
        mount
            .rename_entry(
                NodeId::ROOT,
                OsStr::new("draft"),
                NodeId::ROOT,
                OsStr::new("hello.txt"),
            )
            .expect("rename-over");
        // The old destination inode must still resolve — not ESTALE.
        let attrs = mount
            .attrs(dest_orig.node)
            .expect("orphaned dest inode must remain valid");
        assert_eq!(attrs.kind, NodeKind::File);
    }

    /// A directory rename must rebase the `by_path` mapping for every
    /// already-cached descendant inode (not just the directory's own
    /// record). Otherwise an open handle to `old_dir/leaf` still
    /// references the stale path and post-rename reads return ESTALE
    /// — even though the leaf is reachable through the new directory.
    #[test]
    fn directory_rename_rebases_descendant_inode_paths() {
        let (_temp, mount) = open_mount();
        // Build an overlay-only directory with a leaf so the rename
        // exercises `move_overlay_dir` + the inode rebase pass.
        mount
            .make_dir(NodeId::ROOT, OsStr::new("from_dir"))
            .unwrap();
        let from = mount
            .lookup(NodeId::ROOT, OsStr::new("from_dir"))
            .unwrap()
            .unwrap();
        let leaf = mount
            .create_file(from.node, OsStr::new("leaf.txt"), FileMode::Normal, false)
            .unwrap();
        mount.write(leaf.node, 0, b"payload").unwrap();
        mount.flush(leaf.node).unwrap();
        // Cache the leaf inode by looking it up explicitly — this is
        // what the kernel does for any FD-holding lookup.
        let cached = mount
            .lookup(from.node, OsStr::new("leaf.txt"))
            .unwrap()
            .expect("leaf resolves pre-rename");
        assert_eq!(cached.node, leaf.node);

        mount
            .rename_entry(
                NodeId::ROOT,
                OsStr::new("from_dir"),
                NodeId::ROOT,
                OsStr::new("to_dir"),
            )
            .expect("dir rename");

        // The cached leaf inode must still resolve — its stored path
        // should now be `to_dir/leaf.txt`.
        let mut buf = vec![0u8; 16];
        let n = mount
            .read(cached.node, 0, &mut buf)
            .expect("read via descendant inode after dir rename");
        assert_eq!(&buf[..n], b"payload");
        // And the new path resolves to the same inode.
        let to_dir = mount
            .lookup(NodeId::ROOT, OsStr::new("to_dir"))
            .unwrap()
            .unwrap();
        let via_new = mount
            .lookup(to_dir.node, OsStr::new("leaf.txt"))
            .unwrap()
            .expect("leaf resolves via new dir");
        assert_eq!(via_new.node, cached.node);
    }

    /// `set_attrs(size=0)` truncates the file's hot buffer to zero,
    /// which is what the kernel issues for `O_TRUNC` before any
    /// `write`. cargo writes use `O_CREAT|O_WRONLY|O_TRUNC`; without
    /// this the second build cycle overlays bytes on top of the
    /// previous artifact.
    #[test]
    fn set_attrs_truncate_zero_clears_buffer() {
        let (_temp, mount) = open_mount();
        let node = mount.lookup_path("hello.txt").unwrap();
        // Seed a buffer first.
        mount.write(node, 0, b"world-plus").unwrap();
        let attrs = mount
            .set_attrs(
                node,
                AttrUpdate {
                    size: Some(0),
                    ..Default::default()
                },
            )
            .expect("setattr size=0");
        assert_eq!(attrs.size, 0);
        // Subsequent read returns nothing.
        let mut buf = vec![0u8; 16];
        let n = mount.read(node, 0, &mut buf).unwrap();
        assert_eq!(n, 0);
    }

    /// `set_attrs(size=N)` larger than the current buffer zero-fills
    /// the gap (POSIX `ftruncate(2)`).
    #[test]
    fn set_attrs_truncate_grow_zero_fills() {
        let (_temp, mount) = open_mount();
        let node = mount.lookup_path("hello.txt").unwrap(); // "world", 5 bytes
        let attrs = mount
            .set_attrs(
                node,
                AttrUpdate {
                    size: Some(8),
                    ..Default::default()
                },
            )
            .expect("setattr grow");
        assert_eq!(attrs.size, 8);
        let mut buf = vec![0u8; 16];
        let n = mount.read(node, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"world\0\0\0");
    }

    /// `set_attrs(mode=0o755)` flips a Normal file to Executable in
    /// the overlay; the change is visible on `attrs` immediately and
    /// surfaces in `capture` output (so a freshly-built binary keeps
    /// its `+x` bit).
    #[test]
    fn set_attrs_mode_sets_executable_bit() {
        let (_temp, mount) = open_mount();
        let node = mount.lookup_path("hello.txt").unwrap();
        let attrs = mount
            .set_attrs(
                node,
                AttrUpdate {
                    mode: Some(0o100755),
                    ..Default::default()
                },
            )
            .expect("setattr chmod");
        assert_eq!(attrs.unix_mode & 0o111, 0o111);

        // Refetched attrs preserve the override.
        let again = mount.attrs(node).unwrap();
        assert_eq!(again.unix_mode & 0o111, 0o111);
    }

    /// `enumerate` surfaces overlay symlinks both as standalone
    /// pending-only children (pass-2 `PendingChildKind::Symlink`)
    /// and as overrides on captured-tree entries (pass-1 hit on
    /// `PendingHit::Symlink`). One enumerate exercises both arms.
    #[test]
    fn enumerate_surfaces_overlay_symlinks_in_both_passes() {
        let (_temp, mount) = open_mount();
        // Pass-2 path: brand-new symlink at a name with no captured
        // counterpart.
        mount
            .create_symlink(NodeId::ROOT, OsStr::new("alias"), Path::new("hello.txt"))
            .expect("fresh symlink");
        // Pass-1 path: overlay symlink replaces a captured file.
        // (`run.sh` is in the fixture as an executable file.)
        mount
            .unlink_entry(NodeId::ROOT, OsStr::new("run.sh"))
            .expect("clear captured run.sh");
        mount
            .create_symlink(NodeId::ROOT, OsStr::new("run.sh"), Path::new("hello.txt"))
            .expect("symlink overrides captured file");

        let entries = mount.enumerate(NodeId::ROOT).unwrap();
        let alias = entries
            .iter()
            .find(|e| e.name == "alias")
            .expect("fresh symlink missing from enumerate");
        assert_eq!(alias.kind, NodeKind::Symlink);
        let run = entries
            .iter()
            .find(|e| e.name == "run.sh")
            .expect("overlay symlink override missing from enumerate");
        assert_eq!(run.kind, NodeKind::Symlink);
    }

    /// `create_symlink` records a link in the overlay; `read_link`
    /// returns its target bytes.
    #[test]
    fn create_and_read_symlink() {
        let (_temp, mount) = open_mount();
        let entry = mount
            .create_symlink(
                NodeId::ROOT,
                OsStr::new("alias.txt"),
                Path::new("hello.txt"),
            )
            .expect("symlink");
        assert_eq!(entry.kind, NodeKind::Symlink);
        let target = mount.read_link(entry.node).expect("read_link");
        assert_eq!(target.as_os_str(), OsStr::new("hello.txt"));
    }

    /// `rename_entry` over a symlink in the overlay moves the target
    /// bytes and tombstones the source — `move_symlink`'s overlay-only
    /// path. Existing capture/diff tests don't reach this branch, so
    /// it's the main contributor to uncovered patch lines.
    #[test]
    fn rename_entry_moves_overlay_symlink() {
        let (_temp, mount) = open_mount();
        mount
            .create_symlink(NodeId::ROOT, OsStr::new("alias"), Path::new("hello.txt"))
            .expect("create symlink");
        mount
            .rename_entry(
                NodeId::ROOT,
                OsStr::new("alias"),
                NodeId::ROOT,
                OsStr::new("alias2"),
            )
            .expect("rename symlink");
        assert!(
            mount
                .lookup(NodeId::ROOT, OsStr::new("alias"))
                .unwrap()
                .is_none(),
            "source symlink path must be gone after rename",
        );
        let dst = mount
            .lookup(NodeId::ROOT, OsStr::new("alias2"))
            .unwrap()
            .expect("renamed symlink resolves");
        assert_eq!(dst.kind, NodeKind::Symlink);
        let target = mount.read_link(dst.node).expect("read_link via new path");
        assert_eq!(target.as_os_str(), OsStr::new("hello.txt"));
    }

    /// r11 #4 regression: renaming a regular file over a symlink must
    /// not push an `Orphan { open_count: 0 }` state entry for the
    /// displaced symlink's NodeId. Symlinks have no `open`/`release`
    /// lifecycle, so a state entry there is dead bookkeeping that
    /// nothing will ever reap — it just grows under symlink churn.
    ///
    /// Pre-retrofit (heddle#209) `rename_entry_with_options`'s displaced-
    /// destination branch unconditionally did
    /// `pending.state.insert(displaced_dest, Orphan{ open_count })` even
    /// for non-`Live` nodes (Codex PR #182 r11 finding 3293575541). The
    /// witness-gated retrofit replaces that with a
    /// `BrandedPending::witness_live_nonzero` check whose `None` result
    /// IS the short-circuit — a symlink never enters `state`, so the
    /// witness constructor returns `None` and no transition fires.
    #[test]
    fn rename_over_symlink_does_not_orphan_state() {
        let (_temp, mount) = open_mount();
        let link = mount
            .create_symlink(NodeId::ROOT, OsStr::new("link"), Path::new("hello.txt"))
            .expect("create symlink");
        assert!(
            !mount.orphans_contains(link.node),
            "newly-created symlink must have no Pending state entry",
        );
        mount
            .create_file(NodeId::ROOT, OsStr::new("source"), FileMode::Normal, false)
            .expect("create source file");
        mount
            .rename_entry(
                NodeId::ROOT,
                OsStr::new("source"),
                NodeId::ROOT,
                OsStr::new("link"),
            )
            .expect("rename file over symlink");
        assert!(
            !mount.orphans_contains(link.node),
            "displaced symlink must not acquire a Pending state entry (r11 #4)",
        );
    }

    /// Cross-tree directory rename — i.e. renaming a captured-tree
    /// directory — is intentionally refused by `move_overlay_dir`;
    /// the overlay would otherwise need to rewrite every descendant
    /// tombstone/warm key. Exercises the error path explicitly so
    /// the refusal isn't accidentally relaxed by a later change.
    #[test]
    fn rename_entry_refuses_captured_directory_rename() {
        let (_temp, mount) = open_mount();
        let err = mount
            .rename_entry(
                NodeId::ROOT,
                OsStr::new("nested"),
                NodeId::ROOT,
                OsStr::new("nested2"),
            )
            .expect_err("captured-dir rename must be refused");
        assert!(matches!(err, MountError::InvalidArgument(_)), "got {err:?}");
        assert_eq!(err.to_errno(), libc::EINVAL);
    }

    /// `move_overlay_dir` rebases every overlay slot under the source
    /// dir, but slots OUTSIDE the source must survive unchanged. This
    /// hits the `None` arms of each `rebase` match — the largest
    /// untouched cluster of patch-uncovered lines in this PR.
    #[test]
    fn rename_overlay_dir_preserves_sibling_overlay_state() {
        let (_temp, mount) = open_mount();
        // Two overlay dirs side by side; rename one. The other's
        // children/state — explicit_dirs, warm (via flushed write),
        // a symlink, and a tombstone — must all stay put.
        mount
            .make_dir(NodeId::ROOT, OsStr::new("from_dir"))
            .unwrap();
        mount
            .make_dir(NodeId::ROOT, OsStr::new("keep_dir"))
            .unwrap();
        let keep = mount
            .lookup(NodeId::ROOT, OsStr::new("keep_dir"))
            .unwrap()
            .unwrap();
        // Warm (flushed) leaf under `keep_dir/`.
        let kept_file = mount
            .create_file(keep.node, OsStr::new("warm.txt"), FileMode::Normal, false)
            .unwrap();
        mount.write(kept_file.node, 0, b"persist").unwrap();
        mount.flush(kept_file.node).unwrap();
        // Symlink under `keep_dir/`.
        mount
            .create_symlink(keep.node, OsStr::new("alias"), Path::new("warm.txt"))
            .unwrap();
        // Tombstone the captured `hello.txt` so the tombstone-rebase
        // branch is exercised against a non-prefixed path.
        mount
            .unlink_entry(NodeId::ROOT, OsStr::new("hello.txt"))
            .expect("unlink captured file");

        // The actual rename.
        mount
            .rename_entry(
                NodeId::ROOT,
                OsStr::new("from_dir"),
                NodeId::ROOT,
                OsStr::new("to_dir"),
            )
            .expect("dir rename");

        // The sibling's children survive.
        let keep_after = mount
            .lookup(NodeId::ROOT, OsStr::new("keep_dir"))
            .unwrap()
            .unwrap();
        assert_eq!(keep_after.node, keep.node);
        let warm_after = mount
            .lookup(keep_after.node, OsStr::new("warm.txt"))
            .unwrap()
            .expect("warm leaf must remain at sibling dir");
        let mut buf = vec![0u8; 16];
        let n = mount.read(warm_after.node, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"persist");
        let alias_after = mount
            .lookup(keep_after.node, OsStr::new("alias"))
            .unwrap()
            .expect("sibling symlink must remain");
        assert_eq!(alias_after.kind, NodeKind::Symlink);
        // The unrelated tombstone survives.
        assert!(
            mount
                .lookup(NodeId::ROOT, OsStr::new("hello.txt"))
                .unwrap()
                .is_none(),
            "unrelated tombstone must survive the rename pass",
        );
        // And the rename itself landed.
        assert!(
            mount
                .lookup(NodeId::ROOT, OsStr::new("from_dir"))
                .unwrap()
                .is_none(),
            "source dir must be gone",
        );
        assert!(
            mount
                .lookup(NodeId::ROOT, OsStr::new("to_dir"))
                .unwrap()
                .is_some(),
            "destination dir must be present",
        );
    }

    /// Self-rename (same source and destination) is a POSIX no-op:
    /// returns success without touching any state. Without the early
    /// return, the move + tombstone-source path would actually delete
    /// the file.
    #[test]
    fn rename_entry_self_rename_is_noop() {
        let (_temp, mount) = open_mount();
        mount
            .rename_entry(
                NodeId::ROOT,
                OsStr::new("hello.txt"),
                NodeId::ROOT,
                OsStr::new("hello.txt"),
            )
            .expect("self-rename succeeds");
        // The file remains addressable + readable.
        let hit = mount
            .lookup(NodeId::ROOT, OsStr::new("hello.txt"))
            .unwrap()
            .expect("self-rename did not delete the file");
        let mut buf = vec![0u8; 16];
        let n = mount.read(hit.node, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"world");
    }

    /// POSIX: renaming a directory over a regular file is `ENOTDIR`
    /// (the destination must also be a directory). Catches the
    /// `(Directory, _)` arm of the kind-mismatch guard.
    #[test]
    fn rename_entry_directory_over_file_returns_enotdir() {
        let (_temp, mount) = open_mount();
        mount.make_dir(NodeId::ROOT, OsStr::new("srcdir")).unwrap();
        let err = mount
            .rename_entry(
                NodeId::ROOT,
                OsStr::new("srcdir"),
                NodeId::ROOT,
                OsStr::new("hello.txt"),
            )
            .expect_err("dir-over-file must fail");
        assert!(matches!(err, MountError::NotADirectory(_)), "got {err:?}");
        assert_eq!(err.to_errno(), libc::ENOTDIR);
    }

    /// POSIX: renaming a regular file over a directory is `EISDIR`.
    /// Catches the `(_, Directory)` arm of the kind-mismatch guard.
    #[test]
    fn rename_entry_file_over_directory_returns_eisdir() {
        let (_temp, mount) = open_mount();
        let err = mount
            .rename_entry(
                NodeId::ROOT,
                OsStr::new("hello.txt"),
                NodeId::ROOT,
                OsStr::new("nested"),
            )
            .expect_err("file-over-dir must fail");
        assert!(matches!(err, MountError::IsADirectory(_)), "got {err:?}");
        assert_eq!(err.to_errno(), libc::EISDIR);
    }

    /// POSIX: directory-over-directory rename only succeeds if the
    /// destination is empty; a non-empty destination is `ENOTEMPTY`.
    /// Catches the inner branch of the (Directory, Directory) arm.
    #[test]
    fn rename_entry_directory_over_nonempty_directory_returns_enotempty() {
        let (_temp, mount) = open_mount();
        mount.make_dir(NodeId::ROOT, OsStr::new("srcdir")).unwrap();
        mount.make_dir(NodeId::ROOT, OsStr::new("dstdir")).unwrap();
        let dstdir = mount
            .lookup(NodeId::ROOT, OsStr::new("dstdir"))
            .unwrap()
            .unwrap();
        mount
            .create_file(
                dstdir.node,
                OsStr::new("child.txt"),
                FileMode::Normal,
                false,
            )
            .unwrap();
        let err = mount
            .rename_entry(
                NodeId::ROOT,
                OsStr::new("srcdir"),
                NodeId::ROOT,
                OsStr::new("dstdir"),
            )
            .expect_err("non-empty dest must fail");
        assert!(matches!(err, MountError::NotEmpty(_)), "got {err:?}");
        assert_eq!(err.to_errno(), libc::ENOTEMPTY);
    }

    /// `invalidate` on an orphaned NodeId (an inode the kernel forgets
    /// after `unlink + release`) must retire the orphan tracking entry
    /// — otherwise the `orphans` set accumulates dead IDs across the
    /// session. Indirectly observed via a follow-up unlink+write cycle
    /// on a new NodeId at the same name: that write must take the
    /// normal (republishing) branch, not the orphan branch.
    #[test]
    fn invalidate_clears_orphan_tracking_for_forgotten_inode() {
        let (_temp, mount) = open_mount();
        // Round 1: create, write, unlink — orphans the inode.
        let v1 = mount
            .create_file(NodeId::ROOT, OsStr::new("scratch"), FileMode::Normal, false)
            .unwrap();
        mount.write(v1.node, 0, b"v1").unwrap();
        mount
            .unlink_entry(NodeId::ROOT, OsStr::new("scratch"))
            .unwrap();
        // The kernel issues `release` then `forget`; both flow
        // through `invalidate` in our trait surface.
        mount.invalidate(v1.node).expect("invalidate orphan");

        // Round 2: a fresh inode at the same name. Its writes must
        // republish the path normally — the orphan-cleanup pass in
        // `invalidate` is what keeps the orphan set from making this
        // node mistakenly take the orphan branch.
        let v2 = mount
            .create_file(NodeId::ROOT, OsStr::new("scratch"), FileMode::Normal, false)
            .unwrap();
        assert_ne!(v1.node, v2.node);
        mount.write(v2.node, 0, b"v2-fresh").expect("normal write");
        let hit = mount
            .lookup(NodeId::ROOT, OsStr::new("scratch"))
            .unwrap()
            .expect("recreated path resolves");
        assert_eq!(hit.node, v2.node);
        let mut buf = vec![0u8; 16];
        let n = mount.read(v2.node, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"v2-fresh");
    }

    // --- Codex round 7 findings: orphan-aware write-side ops ----------------
    //
    // r6 fixed `write` so a write through an unlinked-but-still-open fd
    // does not republish the path. Codex r7 surfaced four more write-side
    // ops with the same shape — operations that touch `hot_by_path`,
    // `tombstones`, or path-keyed `warm` entries without consulting the
    // orphan set. Each test below pins one contract from the brief.

    /// `ftruncate` through an unlinked-but-still-open captured fd must
    /// affect only the anonymous open inode. POSIX unlink semantics:
    /// the directory entry stays gone until the last close, and a
    /// flush of the orphan must not warm-promote its buffer. Without
    /// the fix, `apply_truncate`'s tombstone-clear + `hot_by_path`
    /// rebind republished the name to every other observer.
    #[test]
    fn truncate_unlinked_open_doesnt_resurrect_path() {
        let (_temp, mount) = open_mount();
        // `fd = open("hello.txt")` — captured file, no overlay yet.
        let node = mount.lookup_path("hello.txt").unwrap();
        // The `open` above bumps `open_count` to 1 — without this the
        // witness-gated unlink (heddle#209) skips the orphan
        // transition for `Released` (no entry) nodes and the test's
        // open-unlinked POSIX flow doesn't engage.
        mount.on_open(node).expect("on_open");
        // `unlink("hello.txt")` while the handle is still in use.
        mount
            .unlink_entry(NodeId::ROOT, OsStr::new("hello.txt"))
            .expect("unlink");
        assert!(
            mount
                .lookup(NodeId::ROOT, OsStr::new("hello.txt"))
                .unwrap()
                .is_none(),
            "post-unlink lookup must return None"
        );
        // `ftruncate(fd, 2)` through the now-orphaned NodeId.
        mount
            .set_attrs(
                node,
                AttrUpdate {
                    size: Some(2),
                    ..Default::default()
                },
            )
            .expect("ftruncate through unlinked-open fd");
        // Decisive check: the path must still be gone.
        assert!(
            mount
                .lookup(NodeId::ROOT, OsStr::new("hello.txt"))
                .unwrap()
                .is_none(),
            "truncate after unlink must not resurrect the path"
        );
        let entries = mount.enumerate(NodeId::ROOT).unwrap();
        assert!(
            !entries.iter().any(|e| e.name == "hello.txt"),
            "enumerate must not surface the unlinked path: {entries:?}"
        );
        // Flushing the orphan must not warm-promote it — a subsequent
        // capture would otherwise resurrect the path in the captured
        // tree.
        mount.flush(node).expect("flush orphan");
        assert!(
            mount
                .lookup(NodeId::ROOT, OsStr::new("hello.txt"))
                .unwrap()
                .is_none(),
            "post-flush lookup must still be gone (orphan must not warm-promote)"
        );
    }

    /// Rename-over with the destination still open + holding a hot
    /// buffer must preserve the buffer. Reads through the original
    /// NodeId must continue to see the pre-rename bytes. Without the
    /// fix, `move_file`'s blind `pending.hot.remove(&dest_id)` dropped
    /// the buffer, and reads through the still-open fd then routed
    /// through the rebased path overlay and observed the replacement.
    #[test]
    fn rename_over_preserves_replaced_open_fd() {
        let (_temp, mount) = open_mount();
        // Open the captured "hello.txt", write through it, do NOT
        // flush. The hot buffer keyed by dest_id holds "ORIG-DATA".
        let dest_id = mount.lookup_path("hello.txt").unwrap();
        mount
            .write(dest_id, 0, b"ORIG-DATA")
            .expect("write to dest");
        // Build a source file with replacement bytes; flush so the
        // rename's start-of-move flush_node is a no-op.
        let src = mount
            .create_file(NodeId::ROOT, OsStr::new("draft"), FileMode::Normal, false)
            .expect("create src");
        mount
            .write(src.node, 0, b"REPLACE-DATA")
            .expect("write src");
        mount.flush(src.node).expect("flush src");
        // Rename src over dest. dest's pathname is rebound to src;
        // dest's open fd must continue to see "ORIG-DATA".
        mount
            .rename_entry(
                NodeId::ROOT,
                OsStr::new("draft"),
                NodeId::ROOT,
                OsStr::new("hello.txt"),
            )
            .expect("rename-over");
        let mut buf = vec![0u8; 32];
        let n = mount
            .read(dest_id, 0, &mut buf)
            .expect("read via replaced dest fd");
        assert_eq!(
            &buf[..n],
            b"ORIG-DATA",
            "open fd on replaced dest must see pre-rename bytes",
        );
    }

    /// Captured-file rename-over: dest has no hot buffer at the time
    /// of the rename, only the captured-tree blob. Reads/attrs through
    /// the still-open fd must serve the captured bytes, not the
    /// source's replacement. Without the fix, the captured-file branch
    /// of `read` consulted `warm[new_path]` (now the source's data)
    /// before falling through to the captured blob.
    #[test]
    fn rename_over_destination_data_via_old_fd() {
        let (_temp, mount) = open_mount();
        // `fd = open("hello.txt")` — captured file, no overlay.
        let dest_id = mount.lookup_path("hello.txt").unwrap();
        // The `open` above bumps `open_count` to 1 — without this the
        // witness-gated rename-over (heddle#209) skips the orphan
        // transition for `Released` destinations and the
        // captured-bytes-via-old-fd flow doesn't engage.
        mount.on_open(dest_id).expect("on_open");
        // Source file with replacement payload; flush so move_file's
        // flush_node returns immediately.
        let src = mount
            .create_file(NodeId::ROOT, OsStr::new("draft"), FileMode::Normal, false)
            .expect("create src");
        mount
            .write(src.node, 0, b"REPLACE-DATA")
            .expect("write src");
        mount.flush(src.node).expect("flush src");
        mount
            .rename_entry(
                NodeId::ROOT,
                OsStr::new("draft"),
                NodeId::ROOT,
                OsStr::new("hello.txt"),
            )
            .expect("rename-over");
        // Read via the displaced inode id must serve the original
        // captured bytes ("world", 5 bytes) — not the source's
        // "REPLACE-DATA" (12 bytes).
        let mut buf = vec![0u8; 32];
        let n = mount
            .read(dest_id, 0, &mut buf)
            .expect("read via replaced dest fd");
        assert_eq!(
            &buf[..n],
            b"world",
            "open fd on replaced captured dest must see captured bytes, not replacement",
        );
        // attrs must report the captured size too — a stale size from
        // the path overlay would clip the kernel's read buffer.
        let attrs = mount.attrs(dest_id).expect("attrs via replaced dest fd");
        assert_eq!(
            attrs.size, 5,
            "attrs on replaced captured dest must report captured size, not replacement",
        );
    }

    /// `fchmod` on an unlinked-but-still-open fd must affect only the
    /// inode behind that fd. The brief's threat model:
    /// `open(old) → unlink(old) → create(old) → fchmod(orphan_fd, +x)`.
    /// Without the fix, `set_attrs`'s mode mutation updated
    /// `hot_by_path[path]` and `warm[path]` too, flipping the
    /// newly-created file's mode at the same pathname.
    #[test]
    fn fchmod_on_unlinked_open_doesnt_leak_to_recreated() {
        let (_temp, mount) = open_mount();
        // `fd = open("hello.txt")` — captured Normal-mode file.
        let orphan_id = mount.lookup_path("hello.txt").unwrap();
        // The `open` above bumps `open_count` to 1 — without this the
        // witness-gated unlink (heddle#209) skips the orphan
        // transition for `Released` (no entry) nodes and the
        // open-unlinked POSIX flow this test exercises doesn't engage.
        mount.on_open(orphan_id).expect("on_open");
        // `unlink("hello.txt")` while the fd lives on.
        mount
            .unlink_entry(NodeId::ROOT, OsStr::new("hello.txt"))
            .expect("unlink");
        // `create("hello.txt")` mints a fresh inode at the same
        // pathname. Writing some bytes guarantees the hot buffer
        // exists so a leak via `hot_by_path[path]` is visible.
        let fresh = mount
            .create_file(
                NodeId::ROOT,
                OsStr::new("hello.txt"),
                FileMode::Normal,
                false,
            )
            .expect("recreate at same name");
        assert_ne!(orphan_id, fresh.node);
        mount
            .write(fresh.node, 0, b"REBORN")
            .expect("write to recreated file");
        // `fchmod(orphan_fd, 0o755)` — the orphan inode's mode flip
        // must not propagate to the fresh inode at the same pathname.
        mount
            .set_attrs(
                orphan_id,
                AttrUpdate {
                    mode: Some(0o100755),
                    ..Default::default()
                },
            )
            .expect("chmod on orphan");
        // Drive the recreated file through flush + capture so the
        // buffer's `mode` field — which gets persisted into the
        // warm-tier entry and ultimately the captured tree — is
        // observable. A leak shows up as Executable at the new
        // path; the contract says Normal.
        mount.flush(fresh.node).expect("flush fresh");
        let change_id = mount.capture(Some("orphan chmod".into())).unwrap();
        let store = mount.repo_handle().store();
        let state = store.get_state(&change_id).unwrap().unwrap();
        let root_tree = store.get_tree(&state.tree).unwrap().unwrap();
        let entry = root_tree.get("hello.txt").expect("recreated file in tree");
        assert!(
            matches!(entry.mode, objects::object::FileMode::Normal),
            "chmod on orphan must not leak to recreated file: got mode {:?}",
            entry.mode,
        );
    }

    // --- Codex round 8 findings: 3-axis sweep (warm tier + lifecycle + atomicity)
    //
    // r7 made the write-side ops orphan-aware against `pending.hot` /
    // `hot_by_path` / `tombstones` / `inodes.by_path`. r8 closes the
    // remaining three same-shape misses:
    //
    //   1. `pending.warm` was dropped unconditionally on unlink and
    //      rename-over, even when the inode had surviving open fds. The
    //      bytes those fds want to read disappeared with the path.
    //   2. The orphan marker cleared on the FIRST `flush`. FUSE `flush`
    //      fires on EACH descriptor close (incl. `dup`-derived fds);
    //      only `release` is the last-close-per-inode signal. A
    //      premature clear lets a surviving fd's next write republish
    //      the unlinked path.
    //   3. `RENAME_NOREPLACE` did the existence-check in the FUSE shell
    //      and the rename in the core — separate locks, classic TOCTOU.
    //      Concurrent writers could create the destination between the
    //      two operations and the rename would clobber it.

    /// An open fd to a captured file whose warm-tier bytes are present
    /// must still serve those bytes after the path is unlinked. POSIX
    /// open-unlinked semantics: the inode survives behind the fd, and
    /// `pending.warm[path]` is the most-recently-promoted bytes the fd
    /// owns. r7's `unlink_entry` dropped `pending.warm[path]`
    /// unconditionally — orphan reads through the surviving fd lost
    /// the latest writes.
    #[test]
    fn unlink_then_read_warm_via_open_fd() {
        let (_temp, mount) = open_mount();
        // Open hello.txt and write through it. Flush promotes to warm
        // so the bytes are at `pending.warm["hello.txt"]`, not in any
        // hot buffer.
        let node = mount.lookup_path("hello.txt").unwrap();
        // The `open` above bumps `open_count` to 1 — without this the
        // witness-gated unlink (heddle#209) sees the node as
        // `Released` (no entry) and skips the orphan transition.
        mount.on_open(node).expect("on_open");
        mount.write(node, 0, b"WARM-BYTES").expect("write");
        mount.flush(node).expect("flush — promote to warm");
        // Sanity: warm tier holds the bytes.
        assert!(
            mount.warm_blob("hello.txt").is_some(),
            "warm should hold the flushed bytes"
        );
        // `unlink("hello.txt")` while the fd lives on. POSIX: the
        // inode survives; the directory entry is gone.
        mount
            .unlink_entry(NodeId::ROOT, OsStr::new("hello.txt"))
            .expect("unlink");
        // Read via the orphaned fd. The most recent durable bytes for
        // this inode are the warm-tier bytes; they must still be
        // reachable.
        let mut buf = vec![0u8; 32];
        let n = mount
            .read(node, 0, &mut buf)
            .expect("read via orphan fd after warm-promoted unlink");
        assert_eq!(
            &buf[..n],
            b"WARM-BYTES",
            "orphan read must serve preserved warm bytes, not captured fallback"
        );
        // attrs must report the warm size too — a stale captured size
        // would clip the kernel's read buffer.
        let attrs = mount.attrs(node).expect("attrs via orphan fd");
        assert_eq!(
            attrs.size, 10,
            "attrs on orphan must report warm size, not captured"
        );
    }

    /// `ftruncate` through an unlinked-but-still-open fd whose only
    /// durable bytes are warm-tier (no captured-tree predecessor) must
    /// seed the truncated buffer from those warm bytes. Without the
    /// warm-preservation fix, the seed lookup falls through to "empty"
    /// and the surviving fd's `ftruncate` followed by `read` returns
    /// zeros — POSIX requires the truncate to start from the inode's
    /// current bytes.
    #[test]
    fn truncate_unlinked_open_keeps_warm_bytes() {
        let (_temp, mount) = open_mount();
        // Pending file (no captured-tree backing) so the orphan has
        // nothing but warm bytes to fall back on.
        let entry = mount
            .create_file(NodeId::ROOT, OsStr::new("scratch"), FileMode::Normal, false)
            .expect("create");
        // The `create` (`O_CREAT|O_RDWR`) above bumps `open_count` to
        // 1 — without this the witness-gated unlink (heddle#209) sees
        // the node as `Released` (no entry) and skips the orphan
        // transition, breaking the open-unlinked POSIX flow.
        mount.on_open(entry.node).expect("on_open");
        mount.write(entry.node, 0, b"hello-world").expect("write");
        mount.flush(entry.node).expect("flush — promote to warm");
        assert!(
            mount.warm_blob("scratch").is_some(),
            "warm should hold the flushed bytes"
        );
        // Unlink while the fd lives on.
        mount
            .unlink_entry(NodeId::ROOT, OsStr::new("scratch"))
            .expect("unlink");
        // ftruncate(fd, 5) through the orphaned NodeId.
        mount
            .set_attrs(
                entry.node,
                AttrUpdate {
                    size: Some(5),
                    ..Default::default()
                },
            )
            .expect("ftruncate orphan");
        // Read the truncated bytes. Without the warm-preservation
        // fix, this returns "\0\0\0\0\0" instead of "hello".
        let mut buf = vec![0u8; 16];
        let n = mount
            .read(entry.node, 0, &mut buf)
            .expect("read truncated orphan");
        assert_eq!(
            &buf[..n],
            b"hello",
            "truncate-then-read on orphan must seed from preserved warm bytes"
        );
        // Path stays gone.
        assert!(
            mount
                .lookup(NodeId::ROOT, OsStr::new("scratch"))
                .unwrap()
                .is_none(),
            "truncate after unlink must not resurrect the path"
        );
    }

    /// Rename-over when the displaced destination has warm-tier bytes
    /// (no in-flight hot buffer) must preserve those bytes for the
    /// still-open fd. Without the fix, `move_file`'s blind
    /// `pending.warm.remove(new_path)` dropped them; the surviving fd
    /// either sees the source's replacement bytes or an empty file.
    #[test]
    fn rename_over_preserves_warm_for_open_fd() {
        let (_temp, mount) = open_mount();
        // Open hello.txt and write+flush so its bytes live in warm,
        // not hot. r7's existing rename-over preservation fix is for
        // hot buffers; this one exercises the warm-tier preservation.
        let dest_id = mount.lookup_path("hello.txt").unwrap();
        // The `open` above bumps `open_count` to 1 — without this the
        // witness-gated rename-over (heddle#209) skips the orphan
        // transition for `Released` destinations and the
        // warm-preservation path doesn't engage.
        mount.on_open(dest_id).expect("on_open");
        mount
            .write(dest_id, 0, b"DEST-WARM-BYTES")
            .expect("write dest");
        mount.flush(dest_id).expect("flush dest — promote to warm");
        assert!(
            mount.warm_blob("hello.txt").is_some(),
            "dest must be warm-promoted at rename time"
        );
        // Build a source file with replacement payload and flush.
        let src = mount
            .create_file(NodeId::ROOT, OsStr::new("draft"), FileMode::Normal, false)
            .expect("create src");
        mount
            .write(src.node, 0, b"REPLACE-DATA")
            .expect("write src");
        mount.flush(src.node).expect("flush src");
        // Rename src over dest. dest's pathname is rebound to src.
        // dest's open fd must keep seeing the displaced WARM bytes.
        mount
            .rename_entry(
                NodeId::ROOT,
                OsStr::new("draft"),
                NodeId::ROOT,
                OsStr::new("hello.txt"),
            )
            .expect("rename-over");
        // Read via the displaced inode id. Without the fix, this
        // returns "REPLACE-DATA" (source's bytes via `warm[new_path]`)
        // or captured "world" (fallback after warm drop).
        let mut buf = vec![0u8; 32];
        let n = mount
            .read(dest_id, 0, &mut buf)
            .expect("read via replaced dest fd");
        assert_eq!(
            &buf[..n],
            b"DEST-WARM-BYTES",
            "open fd on replaced dest must see pre-rename WARM bytes"
        );
        // attrs must match.
        let attrs = mount.attrs(dest_id).expect("attrs via replaced dest fd");
        assert_eq!(
            attrs.size, 15,
            "attrs must report preserved warm size, not replacement or captured"
        );
    }

    /// FUSE `flush` fires on every descriptor close (including dup'd
    /// fds); only `release` is the last-close-per-inode signal. r7
    /// cleared the orphan marker in `flush_node`, so the FIRST close of
    /// a dup'd unlinked fd cleared the marker prematurely. A subsequent
    /// write through the surviving dup would then take the non-orphan
    /// branch and republish the unlinked path.
    ///
    /// We exercise the contract by simulating the two-open lifecycle:
    /// `on_open` twice (representing 2 fds), `unlink`, `flush` (orphan
    /// marker must stay), `release` once (one fd closed; marker stays),
    /// `release` again (last close; marker clears).
    #[test]
    fn flush_keeps_orphan_marker_until_release() {
        let (_temp, mount) = open_mount();
        // Capture-backed file, opened twice (e.g. one fd then dup —
        // FUSE would track these as a single open + multiple flushes
        // + one release; or two opens + two releases. Either way the
        // marker must persist across the non-final close.)
        let node = mount.lookup_path("hello.txt").unwrap();
        // Two opens → refcount 2.
        mount.on_open(node).expect("open 1");
        mount.on_open(node).expect("open 2");
        // Unlink while both fds live on.
        mount
            .unlink_entry(NodeId::ROOT, OsStr::new("hello.txt"))
            .expect("unlink");
        assert!(
            mount.orphans_contains(node),
            "unlink-while-open must set the orphan marker"
        );
        // `flush` fires on each close (per FUSE protocol). It MUST NOT
        // clear the orphan marker — the surviving fd needs to keep
        // taking the orphan branch on writes.
        mount.flush(node).expect("flush #1");
        assert!(
            mount.orphans_contains(node),
            "flush must not clear the orphan marker"
        );
        // First `release` — represents one of the two fds closing.
        // Marker still present because the other fd holds the inode.
        mount.release(node).expect("release #1");
        assert!(
            mount.orphans_contains(node),
            "non-final release must not clear orphan marker"
        );
        // Second `release` — the last close. Now the marker must
        // clear, the orphan buffer (if any) drops, and the inode's
        // bookkeeping is freed.
        mount.release(node).expect("release #2");
        assert!(
            !mount.orphans_contains(node),
            "final release must clear the orphan marker"
        );
    }

    /// `RENAME_NOREPLACE` must be honoured atomically by the core. r6/r7
    /// did the existence-check in the FUSE shell and the rename in the
    /// core under separate locks — between the two, a concurrent writer
    /// could create the destination and the rename would silently
    /// clobber it. r8 plumbs the flag into the core's mutation
    /// critical section so the check + rename land under the same
    /// write-side lock.
    ///
    /// Deterministic shape: with the flag set, `rename_entry_with_options`
    /// must return `AlreadyExists` when the destination resolves. The
    /// caller is responsible for not racing — but with the core
    /// honouring the flag inside its own critical section, sequential
    /// callers (FUSE serializes write callbacks per inode anyway) get
    /// strict NOREPLACE semantics.
    #[test]
    fn rename_noreplace_is_atomic() {
        use crate::shell::RenameOptions;
        let (_temp, mount) = open_mount();
        // Build a source file with bytes; flush so the rename works
        // entirely from warm tier.
        let src = mount
            .create_file(NodeId::ROOT, OsStr::new("draft"), FileMode::Normal, false)
            .expect("create src");
        mount.write(src.node, 0, b"draft-bytes").expect("write src");
        mount.flush(src.node).expect("flush src");
        // Destination already exists in the captured tree. With
        // NOREPLACE the rename must refuse with `EEXIST`/`AlreadyExists`
        // BEFORE making any mutation.
        let err = mount
            .rename_entry_with_options(
                NodeId::ROOT,
                OsStr::new("draft"),
                NodeId::ROOT,
                OsStr::new("hello.txt"),
                RenameOptions { no_replace: true },
            )
            .expect_err("NOREPLACE over existing dest must fail");
        assert!(
            matches!(err, MountError::AlreadyExists(_)),
            "got unexpected error: {err:?}"
        );
        assert_eq!(err.to_errno(), libc::EEXIST);
        // The source must still be intact — NOREPLACE failure must not
        // leave the source in a half-renamed state.
        let src_after = mount
            .lookup(NodeId::ROOT, OsStr::new("draft"))
            .unwrap()
            .expect("source intact after NOREPLACE rejection");
        let mut buf = vec![0u8; 16];
        let n = mount.read(src_after.node, 0, &mut buf).unwrap();
        assert_eq!(
            &buf[..n],
            b"draft-bytes",
            "source bytes intact after NOREPLACE rejection"
        );
        // And the destination must still be the captured "world".
        let dst = mount
            .lookup(NodeId::ROOT, OsStr::new("hello.txt"))
            .unwrap()
            .expect("dest still resolves");
        let mut buf = vec![0u8; 16];
        let n = mount.read(dst.node, 0, &mut buf).unwrap();
        assert_eq!(
            &buf[..n],
            b"world",
            "dest bytes unchanged after NOREPLACE rejection"
        );

        // Same call WITHOUT the flag must succeed (and replace).
        mount
            .rename_entry_with_options(
                NodeId::ROOT,
                OsStr::new("draft"),
                NodeId::ROOT,
                OsStr::new("hello.txt"),
                RenameOptions::default(),
            )
            .expect("rename without NOREPLACE replaces");
        let dst2 = mount
            .lookup(NodeId::ROOT, OsStr::new("hello.txt"))
            .unwrap()
            .expect("dst still resolves");
        let mut buf = vec![0u8; 32];
        let n = mount.read(dst2.node, 0, &mut buf).unwrap();
        assert_eq!(
            &buf[..n],
            b"draft-bytes",
            "non-NOREPLACE rename replaces dest"
        );
    }

    // --- Codex round 9 finding: hot bytes lost on unlink-of-open ------------
    //
    // r8 closed the warm-tier / lifecycle / atomicity sweep. r9 surfaced a
    // remaining same-shape regression: `unlink_entry` removes
    // `pending.hot[node_id]` for the orphan branch, so an open fd whose only
    // bytes lived in the hot buffer (write then unlink, no flush in between)
    // loses them. Codex thread 3293307302.
    //
    // Under the post-spike unified NodeId-keyed model, hot[node_id] survives
    // the Live → Orphan transition by construction — bytes follow the NodeId,
    // not the path. This test pins the contract: write to an open fd, unlink
    // the path, read through the surviving fd → original bytes.

    /// `open(path) → write(fd, "DIRTY") → unlink(path) → read(fd)` must
    /// return `"DIRTY"`. POSIX open-unlinked semantics: the inode lives
    /// behind the fd until the last close. The pre-spike code dropped
    /// `pending.hot[node_id]` inside `unlink_entry`, so the read fell
    /// through to the captured blob (`"world"`) instead of the dirty hot
    /// bytes the fd had written.
    #[test]
    fn unlink_open_fd_preserves_unflushed_hot_bytes() {
        let (_temp, mount) = open_mount();
        // `fd = open("hello.txt")` — captured file with bytes "world".
        let node = mount.lookup_path("hello.txt").unwrap();
        mount.on_open(node).expect("on_open");
        // Write through the fd, do NOT flush. The bytes live only in
        // `pending.hot[node]`.
        mount
            .write(node, 0, b"DIRTY-BYTES")
            .expect("write through open fd");
        // `unlink("hello.txt")` while the fd lives on.
        mount
            .unlink_entry(NodeId::ROOT, OsStr::new("hello.txt"))
            .expect("unlink while open");
        assert!(
            mount
                .lookup(NodeId::ROOT, OsStr::new("hello.txt"))
                .unwrap()
                .is_none(),
            "post-unlink lookup must be gone"
        );
        // Read via the orphaned fd. The bytes the fd wrote must survive
        // the unlink — POSIX open-unlinked. Pre-fix, hot[node] was
        // dropped at unlink and this returned "world" (captured blob).
        let mut buf = vec![0u8; 32];
        let n = mount.read(node, 0, &mut buf).expect("read via orphan fd");
        assert_eq!(
            &buf[..n],
            b"DIRTY-BYTES",
            "open fd → write → unlink → read must return the unflushed bytes \
             (regression: pre-spike code dropped hot[node_id] in unlink_entry)"
        );
        // attrs must report the dirty-buffer size, not the captured size.
        let attrs = mount.attrs(node).expect("attrs via orphan fd");
        assert_eq!(
            attrs.size, 11,
            "attrs on orphan must report hot-buffer size, not captured"
        );
    }

    // --- Codex round 12 findings: capture / invalidate / forget / rmdir / -----
    // --- symlinks / read_link ------------------------------------------------
    //
    // Six P1 + one P3 findings on the post-spike base (`58a30b2`). Each test
    // pins the contract from one Codex review thread on PR #182.

    /// Codex thread 3293484633 (P1). `capture_with_attribution` clears
    /// `pending.state` unconditionally → drops Orphan tracking for inodes
    /// with still-open fds. Post-capture writes through such an fd then take
    /// the non-orphan branch and republish the (tombstoned) pathname. The
    /// fix retains Orphan entries (and their per-NodeId hot/warm bytes) and
    /// only retires Live entries.
    #[test]
    fn capture_preserves_orphan_state_for_open_inodes() {
        let (_temp, mount) = open_mount();
        // Create a fresh file, open it (refcount = 1), write through the
        // fd, then unlink. The directory entry goes; the inode lives on
        // behind the fd as an Orphan with open_count = 1.
        let entry = mount
            .create_file(NodeId::ROOT, OsStr::new("scratch"), FileMode::Normal, false)
            .expect("create");
        mount.on_open(entry.node).expect("on_open");
        mount.write(entry.node, 0, b"BYTES").expect("write");
        mount
            .unlink_entry(NodeId::ROOT, OsStr::new("scratch"))
            .expect("unlink");
        assert!(
            mount.orphans_contains(entry.node),
            "pre-capture sanity: unlink-while-open must orphan the inode"
        );

        // Capture. Pre-fix, this wipes `state` (and `hot` + `warm`),
        // losing the orphan tracking + the surviving fd's bytes.
        mount
            .capture(Some("capture with orphan open".into()))
            .expect("capture");

        // The fd is still in the kernel's hands; the inode must remain
        // Orphan so a subsequent write through the fd takes the orphan
        // branch and does not republish the deleted pathname.
        assert!(
            mount.orphans_contains(entry.node),
            "capture must preserve Orphan state for inodes with surviving fds"
        );
        mount
            .write(entry.node, 0, b"AFTER")
            .expect("write through orphan fd survives capture");
        assert!(
            mount
                .lookup(NodeId::ROOT, OsStr::new("scratch"))
                .unwrap()
                .is_none(),
            "post-capture write through orphan fd must not republish the path"
        );
        // The per-NodeId hot bytes must also survive capture so reads via
        // the orphan fd serve the inode's own data (POSIX open-unlinked).
        let mut buf = vec![0u8; 32];
        let n = mount
            .read(entry.node, 0, &mut buf)
            .expect("read via orphan fd after capture");
        assert_eq!(
            &buf[..n],
            b"AFTER",
            "orphan hot bytes must survive capture so the fd's own writes are readable"
        );
    }

    /// Codex thread 3293484633 cont. (r11 #2, P1) — heddle#210 retrofit.
    /// `drain_for_capture` used to drop every `Live` entry, including
    /// `Live { open_count >= 1 }`. The open fd's lifecycle row + hot/warm
    /// bytes disappeared, so a subsequent write through that fd
    /// recreated an empty hot buffer — POSIX last-close-wins requires
    /// the fd to keep seeing the bytes it already buffered. The fix
    /// preserves `LiveNonZero` entries + their per-NodeId byte storage.
    #[test]
    fn capture_preserves_live_state_for_open_inodes() {
        let (_temp, mount) = open_mount();
        // Create a fresh file, open it (refcount = 1), write through
        // the fd. NO unlink — the directory entry remains, so the
        // inode stays `Live { open_count: 1 }`.
        let entry = mount
            .create_file(
                NodeId::ROOT,
                OsStr::new("live-and-open"),
                FileMode::Normal,
                false,
            )
            .expect("create");
        mount.on_open(entry.node).expect("on_open");
        mount.write(entry.node, 0, b"BEFORE").expect("write");

        // Capture. Pre-fix this wipes `state` for the Live entry
        // (and its `hot[id]` / `warm[id]`), stranding the kernel fd.
        mount
            .capture(Some("capture with live-and-open fd".into()))
            .expect("capture");

        // Through the surviving fd: a 5-byte overwrite at offset 0
        // must lay over the 6-byte "BEFORE" buffer, NOT a fresh
        // empty one. Pre-fix the drain dropped `hot[node]`, so the
        // post-capture write recreated an empty buffer and the read
        // returned just "AFTER" (5 bytes) — losing the trailing 'E'
        // that POSIX last-close-wins requires the fd to still see.
        // Post-fix the hot buffer survives, the 5-byte overwrite
        // leaves the 6th byte alone, and the read returns "AFTERE".
        mount
            .write(entry.node, 0, b"AFTER")
            .expect("write through live fd survives capture");
        let mut buf = vec![0u8; 32];
        let n = mount
            .read(entry.node, 0, &mut buf)
            .expect("read via live fd after capture");
        assert_eq!(
            &buf[..n],
            b"AFTERE",
            "live hot bytes must survive capture so a partial overwrite via the open fd preserves the trailing pre-capture byte (POSIX last-close-wins; r11 #2)"
        );
    }

    /// Codex threads 3293484634 + 3293510311 (P1). `invalidate` (FUSE
    /// `forget`) drops `pending.warm[node]` unconditionally, but `forget`
    /// only signals that the kernel released its cached inode reference —
    /// not that the file's pending overlay data can be discarded. Warm is
    /// the only durable pre-capture copy of flushed writes; dropping it
    /// silently loses the user's committed-in-session data.
    #[test]
    fn invalidate_preserves_warm_bytes_for_live_inode() {
        let (_temp, mount) = open_mount();
        let entry = mount
            .create_file(NodeId::ROOT, OsStr::new("durable"), FileMode::Normal, false)
            .expect("create");
        mount.write(entry.node, 0, b"DURABLE-WARM").expect("write");
        mount
            .flush(entry.node)
            .expect("flush — promote bytes to warm tier");
        assert!(
            mount.warm_blob("durable").is_some(),
            "pre-invalidate: warm tier holds the flushed bytes"
        );

        // Kernel drops its cached inode reference. The file's path still
        // exists from the user's POV; capture must still plant its bytes.
        mount
            .invalidate(entry.node)
            .expect("invalidate (kernel forget)");

        let change_id = mount
            .capture(Some("post-invalidate capture".into()))
            .expect("capture");
        let bytes = read_captured_blob(&mount, &change_id, "durable");
        assert_eq!(
            &bytes[..],
            b"DURABLE-WARM",
            "warm bytes must survive invalidate so capture can plant them"
        );
    }

    /// Codex PR #182 r11 finding #3 (P1; heddle#211). `invalidate`
    /// (FUSE `forget`) drops `pending.hot[node]` unconditionally,
    /// without first checking whether the inode is still referenced.
    /// For an `Orphan { open_count >= 1 }` node (open-unlinked POSIX
    /// flow), the kernel can issue `forget` for the dentry-side
    /// reference while a userspace fd still holds the inode; dropping
    /// `hot[node]` strands the surviving fd with no readable bytes.
    /// Post-retrofit (heddle#211) the witness-gated
    /// `BrandedPending::kernel_forget_inode` rejects any `Orphan`
    /// state, and the missing witness short-circuits the entire
    /// forget path in `MountInner::invalidate` — leaving `hot[node]`,
    /// `state[node]`, and the inode record intact until the final
    /// `release` retires them.
    #[test]
    fn invalidate_preserves_hot_bytes_for_orphan_with_open_fd() {
        let (_temp, mount) = open_mount();
        // Create + open a fresh file, write through the fd. Bytes
        // stay in `hot[node]` — no flush, so warm is untouched.
        let entry = mount
            .create_file(NodeId::ROOT, OsStr::new("scratch"), FileMode::Normal, false)
            .expect("create");
        mount.on_open(entry.node).expect("on_open");
        mount
            .write(entry.node, 0, b"HOT-BYTES")
            .expect("write through live fd populates hot");
        // Unlink while the fd lives on. POSIX: dentry gone, inode
        // survives behind the fd; FSM transitions to
        // `Orphan { open_count: 1 }`.
        mount
            .unlink_entry(NodeId::ROOT, OsStr::new("scratch"))
            .expect("unlink");
        assert!(
            mount.orphans_contains(entry.node),
            "pre-invalidate sanity: unlink-while-open must orphan the inode"
        );

        // Kernel forget arrives for the dentry-side reference while
        // the fd is still in userspace's hands. Pre-retrofit
        // `invalidate` blindly removed `hot[node]`; post-retrofit
        // the witness rejects `Orphan` and the forget path
        // short-circuits.
        mount
            .invalidate(entry.node)
            .expect("invalidate (kernel forget) on orphan with open fd");

        // Read via the surviving fd. The hot-tier bytes must still
        // be served — the open-unlinked POSIX contract demands that
        // the fd's view of the inode outlives the dentry.
        let mut buf = vec![0u8; 32];
        let n = mount
            .read(entry.node, 0, &mut buf)
            .expect("read via orphan fd after kernel forget");
        assert_eq!(
            &buf[..n],
            b"HOT-BYTES",
            "kernel forget racing an open Orphan fd must not drop \
             hot[node] — the surviving fd needs the bytes (r11 #3)"
        );
    }

    /// Codex thread 3293510310 (P1). `rmdir_entry` plants a
    /// `dir_tombstones` entry but leaves `inodes.by_path[child_path]`
    /// intact. A subsequent `create_file` / `make_dir` at the same path
    /// then coalesces onto the removed directory's NodeId via
    /// `Inodes::intern`'s path-keyed reverse index — that's a stale-handle
    /// class identical to the one `unlink_entry` already guards against
    /// by retiring `by_path`.
    #[test]
    fn rmdir_retires_path_to_inode_binding() {
        let (_temp, mount) = open_mount();
        let dir = mount
            .make_dir(NodeId::ROOT, OsStr::new("scratch"))
            .expect("mkdir");
        mount
            .rmdir_entry(NodeId::ROOT, OsStr::new("scratch"))
            .expect("rmdir");
        // Recreate the name as a regular file. The fresh inode must NOT
        // be the removed directory's NodeId — POSIX remove-and-recreate
        // isolation forbids rebinding a cached dir inode to a different
        // object type.
        let file = mount
            .create_file(NodeId::ROOT, OsStr::new("scratch"), FileMode::Normal, false)
            .expect("recreate as file");
        assert_ne!(
            file.node, dir.node,
            "remove-and-recreate at same path must mint a fresh inode \
             (rmdir must retire inodes.by_path so intern does not coalesce)"
        );
    }

    /// Codex thread 3293510316 (P1). `read_link` on a captured `Symlink`
    /// record reconstructs an `OsStr` with `OsStr::from_encoded_bytes_unchecked`
    /// from blob bytes loaded from the store. That call's safety contract
    /// requires bytes minted by `OsStr::as_encoded_bytes` in *this*
    /// process and Rust version — captured-tree bytes can come from any
    /// process and any version, so the call is unsound.
    ///
    /// On Unix, the platform-safe replacement is `OsStrExt::from_bytes`
    /// (sound for any byte sequence); on Windows, the bytes must be UTF-8
    /// validated and rejected otherwise (the encoding contract for `OsStr`
    /// is process-internal). This test pins the round-trip contract: a
    /// captured symlink's target is recoverable byte-for-byte.
    #[test]
    fn read_link_safe_for_captured_symlink_bytes() {
        let (_temp, mount) = open_mount();
        // Create + capture a symlink so the next read_link goes through
        // the captured-blob branch (the unsound site).
        mount
            .create_symlink(NodeId::ROOT, OsStr::new("link"), Path::new("hello.txt"))
            .expect("create_symlink");
        let _ = mount.capture(Some("plant link".into())).expect("capture");
        let entry = mount
            .lookup(NodeId::ROOT, OsStr::new("link"))
            .unwrap()
            .expect("captured symlink resolves");
        let resolved = mount.read_link(entry.node).expect("read_link");
        assert_eq!(
            resolved,
            std::ffi::OsString::from("hello.txt"),
            "captured symlink must decode via a sound platform-safe path"
        );
    }

    /// Codex thread 3293510317 (P3). `unlink_entry` always transitions
    /// the removed node into `NodeState::Orphan`, but symlinks are not
    /// openable for IO — they never receive `open`/`release` lifecycle
    /// events to clear the state. The entry accumulates in `pending.state`
    /// until capture/invalidate. The fix gates the orphan transition on
    /// `entry.kind != Symlink`.
    #[test]
    fn unlink_of_symlink_does_not_create_orphan_state() {
        let (_temp, mount) = open_mount();
        let link = mount
            .create_symlink(NodeId::ROOT, OsStr::new("link"), Path::new("hello.txt"))
            .expect("create_symlink");
        mount
            .unlink_entry(NodeId::ROOT, OsStr::new("link"))
            .expect("unlink symlink");
        assert!(
            !mount.orphans_contains(link.node),
            "symlinks have no open/release lifecycle; unlink must not orphan them"
        );
    }

    /// Codex thread 3293680448 (P1). `Inodes::forget` unconditionally
    /// removes `by_path[path]` for the retired record. After an
    /// unlink-then-recreate cycle, the stored `path` may be rebound to
    /// a *different* live inode. A late kernel `forget` for the old
    /// inode then deletes the live inode's path binding — capture's
    /// warm-entry path check skips the entry, breaking lookup/read/write
    /// routing for the replacement.
    #[test]
    fn forget_after_unlink_recreate_preserves_new_path_binding() {
        let (_temp, mount) = open_mount();
        // v1 minted and opened. unlink while open → v1 orphans; by_path
        // is detached. v2 created at the same name → mints fresh inode.
        let v1 = mount
            .create_file(NodeId::ROOT, OsStr::new("scratch"), FileMode::Normal, false)
            .expect("create v1");
        mount.on_open(v1.node).expect("on_open v1");
        mount
            .unlink_entry(NodeId::ROOT, OsStr::new("scratch"))
            .expect("unlink v1");
        let v2 = mount
            .create_file(NodeId::ROOT, OsStr::new("scratch"), FileMode::Normal, false)
            .expect("recreate as v2");
        assert_ne!(v1.node, v2.node, "recreate must mint a fresh inode");
        mount.write(v2.node, 0, b"v2-bytes").expect("write v2");
        mount.flush(v2.node).expect("flush v2 to warm");

        // Kernel forgets v1 (the orphan). Pre-fix, `Inodes::forget`'s
        // blind `by_path.remove("scratch")` wipes v2's binding.
        mount.invalidate(v1.node).expect("forget v1");

        // Lookup via warm must still resolve via v2.
        let hit = mount
            .lookup(NodeId::ROOT, OsStr::new("scratch"))
            .unwrap()
            .expect("scratch still resolves after old-inode forget");
        assert_eq!(hit.node, v2.node, "lookup must return v2's NodeId");

        // Capture must plant v2's bytes. Pre-fix the by_path binding is
        // wiped → apply_pending_to_tree's path check skips v2 → captured
        // tree is missing scratch.
        let change_id = mount
            .capture(Some("post-forget capture".into()))
            .expect("capture");
        let captured = read_captured_blob(&mount, &change_id, "scratch");
        assert_eq!(
            &captured[..],
            b"v2-bytes",
            "captured tree must contain v2's bytes — forget of the old \
             inode must not wipe the new inode's path binding"
        );
    }

    // --- Codex round 13 findings: setattr lock discipline + name/mode pickiness
    //
    // r12 closed the post-spike NodeId-keyed refactor's residual surgical
    // gaps. r13 surfaces three more same-class issues that all live in the
    // write-side `set_attrs` / `validate_entry_name` surface:
    //
    //   1. `set_attrs(size=...)` routes into `apply_truncate` without taking
    //      `write_mu`. Concurrent with `rename`, the truncate's final
    //      bookkeeping uses the pre-rename pathname — republishing
    //      `hot_by_path[old]` and clearing the rename's tombstone, so the
    //      old name resurrects. (Codex thread 3293733165, P1.)
    //   2. `validate_entry_name` rejects only NUL and `/`, but the tree
    //      serializer additionally rejects `\` and control bytes
    //      (0x01..=0x1F, 0x7F). Names that pass the FUSE-side check then
    //      fail at capture with a confusing "invalid object" error rather
    //      than a clean EINVAL at write time. (Codex thread 3293733163, P2.)
    //   3. `set_attrs`'s Normal↔Executable fold treats any of the three
    //      execute bits as executable (`mode & 0o111 != 0`). The contract
    //      is "user execute only" — a `chmod 0o010` (group execute) must
    //      stay Normal. (Codex thread 3293733164, P2.)

    /// Codex r13 thread 3293733165 (P1). `set_attrs(size=...)` racing
    /// with `rename_entry` must not republish the pre-rename pathname.
    ///
    /// Mechanism without `write_mu` in `set_attrs`: `apply_truncate`
    /// captures `path` at the top via `record_for`, then drops every
    /// lock for the seed `load_blob_bytes` call. A concurrent rename
    /// fits entirely in that lock-free window — it inserts
    /// `tombstones[old]`, removes `hot_by_path[old]`, and rebases the
    /// inode's stored path. When `apply_truncate`'s phase-2 mutation
    /// re-acquires the pending lock and runs
    /// `tombstones.remove(&path) + hot_by_path.insert(path, node)`
    /// with the stale `path = old`, the rename's tombstone is wiped
    /// and `lookup(old)` resurrects the file.
    ///
    /// Stress shape: 200 trials, two threads sync on a barrier and
    /// then run their op concurrently. Even a single observation of
    /// `lookup(old).is_some()` after both complete fails the test.
    /// With the fix (`write_mu` held around the mutating `set_attrs`
    /// paths), the race is structurally impossible.
    #[test]
    fn setattr_truncate_serializes_against_rename() {
        use std::{
            sync::{Arc, Barrier},
            thread,
        };
        const TRIALS: usize = 200;
        let mut resurrected: Vec<usize> = Vec::new();
        for trial in 0..TRIALS {
            let (_temp, mount) = open_mount();
            let mount = Arc::new(mount);
            // `hello.txt` is a captured 5-byte file; `apply_truncate`'s
            // NeedSeed branch will fetch the blob and widen the
            // lock-free window during the race.
            let node = mount.lookup_path("hello.txt").unwrap();
            let barrier = Arc::new(Barrier::new(2));

            let b_a = barrier.clone();
            let m_a = mount.clone();
            let h_a = thread::spawn(move || {
                b_a.wait();
                m_a.set_attrs(
                    node,
                    AttrUpdate {
                        size: Some(2),
                        ..Default::default()
                    },
                )
                .expect("setattr(size=2)");
            });
            let b_b = barrier.clone();
            let m_b = mount.clone();
            let h_b = thread::spawn(move || {
                b_b.wait();
                m_b.rename_entry(
                    NodeId::ROOT,
                    OsStr::new("hello.txt"),
                    NodeId::ROOT,
                    OsStr::new("renamed.txt"),
                )
                .expect("rename");
            });
            h_a.join().unwrap();
            h_b.join().unwrap();

            // Invariant: regardless of interleaving, the pre-rename
            // path must not resolve after both ops complete.
            if mount
                .lookup(NodeId::ROOT, OsStr::new("hello.txt"))
                .unwrap()
                .is_some()
            {
                resurrected.push(trial);
            }
        }
        assert!(
            resurrected.is_empty(),
            "setattr(size)+rename race resurrected pre-rename path \
             in {} of {} trials (trials: {:?})",
            resurrected.len(),
            TRIALS,
            resurrected,
        );
    }

    /// Codex r13 thread 3293733163 (P2). `validate_entry_name` must
    /// reject every name the tree serializer would reject — otherwise
    /// the overlay accepts a create/rename that later blows up at
    /// `capture` with a confusing "invalid object" error.
    ///
    /// The tree serializer's rule (objects::object::tree_entry::
    /// validate_name) rejects: empty, `.`, `..`, anything containing
    /// `/` or `\`, anything with a byte < 0x20 or == 0x7F. The mount's
    /// `validate_entry_name` previously rejected only NUL + `/`.
    ///
    /// Each name in this list pins one rule the mount must enforce up
    /// front. Without the fix, `create_file` / `rename` accept these
    /// and the failure surfaces later (or not at all, leaking a stale
    /// pending entry).
    #[test]
    fn create_rejects_names_tree_cannot_serialize() {
        let (_temp, mount) = open_mount();
        // Each (name, why) tuple is rejected by the tree serializer.
        let cases: &[(&[u8], &str)] = &[
            (b"with\\backslash", "backslash (path separator on Windows)"),
            (b"bel\x07", "BEL (0x07) is a control char"),
            (b"esc\x1b", "ESC (0x1B) is a control char"),
            (b"tab\there", "TAB (0x09) is a control char"),
            (b"newline\nhere", "LF (0x0A) is a control char"),
            (b"cr\rhere", "CR (0x0D) is a control char"),
            (b"del\x7f", "DEL (0x7F) is the upper control char"),
        ];
        for (bytes, why) in cases {
            // Build the OsStr from raw bytes so we can include non-UTF-8
            // tricks (the safe path here is all-ASCII, but the helper
            // is byte-oriented to match the tree serializer's reject
            // set).
            #[cfg(unix)]
            let name = {
                use std::os::unix::ffi::OsStrExt;
                std::ffi::OsString::from(OsStr::from_bytes(bytes))
            };
            #[cfg(not(unix))]
            let name =
                std::ffi::OsString::from(std::str::from_utf8(bytes).expect("ascii test inputs"));

            let err = mount
                .create_file(NodeId::ROOT, &name, FileMode::Normal, false)
                .expect_err(&format!(
                    "create_file must reject {name:?} ({why}) up front"
                ));
            assert!(
                matches!(err, MountError::InvalidArgument(_)),
                "expected InvalidArgument for {name:?} ({why}), got {err:?}"
            );
        }
    }

    /// Codex r13 thread 3293733164 (P2). The Normal↔Executable mode
    /// fold must trigger on the user execute bit (S_IXUSR = 0o100)
    /// only, not on any of the three execute bits (0o111). A
    /// `chmod 0o010` (group execute only) must NOT promote a Normal
    /// file to Executable — that would unexpectedly grant owner
    /// execute at capture time.
    #[test]
    fn chmod_group_execute_alone_doesnt_promote_to_executable() {
        let (_temp, mount) = open_mount();
        let node = mount.lookup_path("hello.txt").unwrap();
        // chmod 0o010: group execute only. Per the contract, this
        // must NOT promote to Executable.
        let attrs = mount
            .set_attrs(
                node,
                AttrUpdate {
                    mode: Some(0o100_010),
                    ..Default::default()
                },
            )
            .expect("chmod 0o010");
        // FileMode::Normal serializes to 0o644 (no execute anywhere);
        // FileMode::Executable serializes to 0o755 (all execute bits).
        // The fix gates the fold on S_IXUSR (0o100), so 0o010 leaves
        // the record as Normal → unix_mode has no execute bits at all.
        assert_eq!(
            attrs.unix_mode & 0o111,
            0,
            "chmod 0o010 must NOT promote to Executable (got unix_mode={:o})",
            attrs.unix_mode
        );

        // Companion assertion (anti-regression): chmod 0o100 (user
        // execute only) DOES promote to Executable.
        let attrs = mount
            .set_attrs(
                node,
                AttrUpdate {
                    mode: Some(0o100_100),
                    ..Default::default()
                },
            )
            .expect("chmod 0o100");
        assert_eq!(
            attrs.unix_mode & 0o111,
            0o111,
            "chmod 0o100 must promote to Executable (got unix_mode={:o})",
            attrs.unix_mode
        );

        // And chmod 0o755 (the canonical Executable path) still
        // works — this is the path the r12 baseline already covers
        // and we don't want to regress.
        let attrs = mount
            .set_attrs(
                node,
                AttrUpdate {
                    mode: Some(0o100_755),
                    ..Default::default()
                },
            )
            .expect("chmod 0o755");
        assert_eq!(
            attrs.unix_mode & 0o111,
            0o111,
            "chmod 0o755 must keep Executable (got unix_mode={:o})",
            attrs.unix_mode
        );
    }
}

// D1. Property test for two-tier semantics.
mod proptests {
    use std::collections::BTreeMap;

    use proptest::prelude::*;

    use super::*;
    use crate::core::test_helpers::install_pending_file;

    /// One operation in a randomly-generated trace.
    #[derive(Clone, Debug)]
    enum Op {
        /// Apply a `pwrite(name, offset, bytes)`. Mirrors POSIX
        /// `pwrite`: bytes outside `[offset, offset+bytes.len())`
        /// are preserved; if `offset > current_len`, the gap is
        /// zero-filled before the write lands.
        WriteFresh {
            name: String,
            offset: u64,
            bytes: Vec<u8>,
        },
        /// Flush any open buffer for `name` (no-op if none open).
        Flush { name: String },
        /// Read up to `len` bytes at offset 0 from `name`.
        Read { name: String, len: usize },
        /// Capture the pending tier into a fresh state.
        Capture,
        /// Unlink `name`. Drops any in-flight buffer, the warm
        /// entry, and plants a tombstone so the captured-tier entry
        /// (if any) is hidden. POSIX: a subsequent `open(O_CREAT)`
        /// + `pwrite` reborns the path as a fresh file.
        Unlink { name: String },
    }

    fn strategy_name() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("a.txt".to_string()),
            Just("b.txt".to_string()),
            Just("nested/c.txt".to_string()),
            Just("nested/d.txt".to_string()),
        ]
    }

    fn strategy_op() -> impl Strategy<Value = Op> {
        prop_oneof![
            // Writes always carry at least one byte — a zero-length
            // write on the heddle mount allocates an (empty) hot
            // buffer that shadows the captured-tree blob, semantics
            // worth revisiting in production but not the focus of
            // this property test.
            //
            // Offsets are bounded to `0..32`. That's larger than
            // most write sizes (1..32) so we exercise both
            // partial-overwrite and write-past-EOF zero-fill.
            (
                strategy_name(),
                0u64..32,
                proptest::collection::vec(any::<u8>(), 1..32)
            )
                .prop_map(|(name, offset, bytes)| Op::WriteFresh {
                    name,
                    offset,
                    bytes,
                }),
            strategy_name().prop_map(|name| Op::Flush { name }),
            (strategy_name(), 1usize..64usize).prop_map(|(name, len)| Op::Read { name, len }),
            Just(Op::Capture),
            // Unlinks share the same name pool as writes, so the
            // generator naturally exercises unlink-then-write,
            // write-then-unlink, capture-after-unlink, and
            // unlink-then-capture-then-write interleavings.
            strategy_name().prop_map(|name| Op::Unlink { name }),
        ]
    }

    /// Apply POSIX `pwrite` semantics to a model `Vec<u8>`: extend
    /// with zeros if the offset is past the end, then overwrite the
    /// `[offset, offset+data.len())` window. Bytes beyond that
    /// window are preserved verbatim.
    fn model_pwrite(buf: &mut Vec<u8>, offset: usize, data: &[u8]) {
        let end = offset + data.len();
        if buf.len() < end {
            buf.resize(end, 0);
        }
        buf[offset..end].copy_from_slice(data);
    }

    proptest! {
        // 256 cases per the spec; each opens a fresh repo+mount
        // per case so the test wall-clock is dominated by repo
        // setup cost (tens of ms each). Override locally if you
        // want to widen the search; CI keeps the default.
        #![proptest_config(ProptestConfig {
            cases: 256,
            .. ProptestConfig::default()
        })]
        #[test]
        fn mount_matches_in_memory_model(ops in proptest::collection::vec(strategy_op(), 1..32)) {
            let temp = TempDir::new().unwrap();
            let repo = Repository::init_default(temp.path()).unwrap();
            // Disable the clock sweep so `idle_after` doesn't race
            // with the model.
            let mount = ContentAddressedMount::new(repo, "main").unwrap().with_promotion_policy(
                crate::core::PromotionPolicy {
                    idle_after: std::time::Duration::from_secs(3600),
                    sweep_interval: None,
                },
            );
            // Track open file nodes so Flush can find them.
            let mut open: BTreeMap<String, NodeId> = BTreeMap::new();
            // Reference model: path -> current bytes. Mirrors POSIX
            // semantics — partial overwrites preserve the tail,
            // writes past EOF zero-fill the gap. After `Capture`,
            // the model stays as-is; the pending tier resets but
            // the captured state represents the same per-path
            // bytes.
            let mut model: BTreeMap<String, Vec<u8>> = BTreeMap::new();

            for op in ops {
                match op {
                    Op::WriteFresh { name, offset, bytes } => {
                        // If the path is already in the captured
                        // tree (from a prior capture), open via
                        // component-walk lookup instead of minting
                        // a fresh PendingFile (which would shadow
                        // the captured entry's identity).
                        let node = if let Some(entry) =
                            lookup_path_via_components(&mount, &name)
                        {
                            entry.node
                        } else {
                            install_pending_file(
                                &mount,
                                &name,
                                objects::object::FileMode::Normal,
                            )
                        };
                        mount.write(node, offset, &bytes).unwrap();
                        // Stay open across writes so subsequent
                        // pwrites to the same path coalesce on the
                        // same hot buffer. `Op::Flush` is the only
                        // promotion path; we no longer auto-flush.
                        open.insert(name.clone(), node);
                        let buf = model.entry(name).or_default();
                        model_pwrite(buf, offset as usize, &bytes);
                    }
                    Op::Flush { name } => {
                        if let Some(node) = open.remove(&name) {
                            mount.flush(node).unwrap();
                        }
                    }
                    Op::Read { name, len } => {
                        let lookup = lookup_path_via_components(&mount, &name);
                        match (lookup, model.get(&name)) {
                            (Some(entry), Some(model_bytes)) => {
                                let mut buf = vec![0u8; len];
                                let n = mount.read(entry.node, 0, &mut buf).unwrap();
                                let take = std::cmp::min(len, model_bytes.len());
                                prop_assert_eq!(&buf[..n], &model_bytes[..take]);
                            }
                            (None, None) => {}
                            (Some(_), None) => prop_assert!(false, "mount has {} but model doesn't", name),
                            (None, Some(_)) => prop_assert!(false, "model has {} but mount doesn't", name),
                        }
                    }
                    Op::Capture => {
                        let _ = mount.capture(Some("proptest".into())).unwrap();
                        // After capture the pending tier is empty,
                        // but the captured tree still resolves the
                        // model paths through `lookup`. Open buffers
                        // also reset.
                        open.clear();
                    }
                    Op::Unlink { name } => {
                        // Tombstone the path on the mount and remove
                        // it from the model. POSIX: any open fd we
                        // were holding becomes orphaned — a fresh
                        // `Op::WriteFresh` for the same name must
                        // mint a new PendingFile (since lookup will
                        // return None) and start the file from
                        // scratch. Drop our `open` entry so a
                        // subsequent Flush is a no-op rather than
                        // touching the orphaned NodeId.
                        mount.unlink_path(&name).unwrap();
                        open.remove(&name);
                        model.remove(&name);
                    }
                }
                // Per-step invariant: every path the mount can resolve
                // (via component-walk) must read back what the model
                // says. Catches tombstones-vs-hot-buffer mismatches and
                // capture-after-unlink leaks immediately at the offending
                // step rather than at end-of-trace.
                for path in [
                    "a.txt",
                    "b.txt",
                    "nested/c.txt",
                    "nested/d.txt",
                ] {
                    let entry = lookup_path_via_components(&mount, path);
                    match (entry, model.get(path)) {
                        (Some(e), Some(model_bytes)) => {
                            let mut buf = vec![0u8; model_bytes.len().max(1)];
                            let n = mount.read(e.node, 0, &mut buf).unwrap();
                            prop_assert_eq!(
                                &buf[..n],
                                &model_bytes[..],
                                "mount/model mismatch on read of {}",
                                path
                            );
                        }
                        (None, None) => {}
                        (Some(_), None) => prop_assert!(
                            false,
                            "mount resolved {} but model has no entry (tombstone leak?)",
                            path
                        ),
                        (None, Some(_)) => prop_assert!(
                            false,
                            "model has {} but mount lookup returned None (tombstone obscures hot/warm?)",
                            path
                        ),
                    }
                }
            }
            // Final consistency check: every model path must resolve
            // through component-walk lookup, and read the right bytes.
            // Conversely, every name in the pool that the model says is
            // gone must NOT resolve in the mount (no zombie tombstones).
            for path in [
                "a.txt",
                "b.txt",
                "nested/c.txt",
                "nested/d.txt",
            ] {
                let entry = lookup_path_via_components(&mount, path);
                match (entry, model.get(path)) {
                    (Some(e), Some(bytes)) => {
                        let mut buf = vec![0u8; bytes.len().max(1)];
                        let n = mount.read(e.node, 0, &mut buf).unwrap();
                        prop_assert_eq!(&buf[..n], &bytes[..]);
                    }
                    (None, None) => {}
                    (Some(_), None) => prop_assert!(
                        false,
                        "final: mount resolved {} but model deleted it",
                        path
                    ),
                    (None, Some(_)) => prop_assert!(
                        false,
                        "final: model has {} but mount returned None",
                        path
                    ),
                }
            }
        }
    }
}

// D2. Real-FUSE Linux integration test.
#[cfg(all(target_os = "linux", feature = "fuse"))]
mod fuse_smoke {
    use std::time::Duration;

    use super::*;
    use crate::FuseShell;

    fn fuse_available() -> bool {
        std::path::Path::new("/dev/fuse").exists()
    }

    /// Wait for `path` to come up, bounded by `deadline`.
    fn wait_until_exists(path: &std::path::Path, deadline: Duration) {
        let start = std::time::Instant::now();
        while !path.exists() && start.elapsed() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn fuse_open_close_read_round_trip() {
        if !fuse_available() {
            eprintln!("skipping: /dev/fuse not present");
            return;
        }
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        std::fs::write(repo_dir.path().join("seed.txt"), b"hello").unwrap();
        repo.snapshot(Some("seed".into()), None).unwrap();

        let mount = ContentAddressedMount::new(repo, "main").unwrap();
        let mountpoint = TempDir::new().unwrap();
        let session = match FuseShell::new(mount).mount_background(mountpoint.path()) {
            Ok(s) => s,
            Err(_) => {
                eprintln!("skipping: FUSE mount failed (likely no kernel module)");
                return;
            }
        };
        wait_until_exists(&mountpoint.path().join("seed.txt"), Duration::from_secs(5));
        let read = std::fs::read_to_string(mountpoint.path().join("seed.txt")).unwrap();
        assert_eq!(read, "hello");
        drop(session);
    }
}

// ---------------------------------------------------------------------------
// PlatformShell default-impl coverage.
//
// Every write-side method on the trait has a default body that returns
// `MountError::ReadOnly`. A read-only adapter (e.g. a future "snapshot
// browser" shell that only implements the six required methods) inherits
// those defaults. These tests exercise the default bodies so the contract
// stays observable: read-only shells uniformly surface `ReadOnly` rather
// than panicking or silently succeeding.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod platform_shell_defaults {
    use std::{
        ffi::{OsStr, OsString},
        path::Path,
        time::SystemTime,
    };

    use objects::object::FileMode;

    use crate::{
        error::{MountError, Result},
        shell::{AttrUpdate, Attrs, Entry, NodeId, NodeKind, PlatformShell},
    };

    /// Minimal read-only shell that implements only the required
    /// methods. Every write-side default kicks in.
    struct ReadOnlyStub;

    impl PlatformShell for ReadOnlyStub {
        fn lookup(&self, _parent: NodeId, _name: &OsStr) -> Result<Option<Entry>> {
            Ok(None)
        }
        fn read(&self, _node: NodeId, _offset: u64, _buf: &mut [u8]) -> Result<usize> {
            Ok(0)
        }
        fn write(&self, _node: NodeId, _offset: u64, _data: &[u8]) -> Result<usize> {
            Err(MountError::ReadOnly)
        }
        fn enumerate(&self, _dir: NodeId) -> Result<Vec<Entry>> {
            Ok(vec![])
        }
        fn attrs(&self, _node: NodeId) -> Result<Attrs> {
            Ok(Attrs {
                node: NodeId::ROOT,
                kind: NodeKind::Directory,
                size: 0,
                unix_mode: 0o040755,
                nlink: 1,
                mtime: SystemTime::UNIX_EPOCH,
            })
        }
        fn invalidate(&self, _node: NodeId) -> Result<()> {
            Ok(())
        }
    }

    fn is_readonly<T>(r: Result<T>) -> bool {
        matches!(r, Err(MountError::ReadOnly))
    }

    #[test]
    fn defaults_uniformly_surface_readonly() {
        let s = ReadOnlyStub;
        assert!(is_readonly(s.create_file(
            NodeId::ROOT,
            OsStr::new("x"),
            FileMode::Normal,
            false,
        )));
        assert!(is_readonly(s.make_dir(NodeId::ROOT, OsStr::new("d"))));
        assert!(is_readonly(s.unlink_entry(NodeId::ROOT, OsStr::new("x"))));
        assert!(is_readonly(s.rmdir_entry(NodeId::ROOT, OsStr::new("d"))));
        assert!(is_readonly(s.rename_entry(
            NodeId::ROOT,
            OsStr::new("a"),
            NodeId::ROOT,
            OsStr::new("b"),
        )));
        assert!(is_readonly(
            s.set_attrs(NodeId::ROOT, AttrUpdate::default())
        ));
        assert!(is_readonly(s.create_symlink(
            NodeId::ROOT,
            OsStr::new("ln"),
            Path::new("target"),
        )));
        let read_link_result: Result<OsString> = s.read_link(NodeId::ROOT);
        assert!(is_readonly(read_link_result));
    }

    /// The default `release` body delegates to `flush`. A shell that
    /// overrides only `flush` should see `release` follow through.
    #[test]
    fn release_default_delegates_to_flush() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct CountFlush(AtomicUsize);
        impl PlatformShell for CountFlush {
            fn lookup(&self, _p: NodeId, _n: &OsStr) -> Result<Option<Entry>> {
                Ok(None)
            }
            fn read(&self, _n: NodeId, _o: u64, _b: &mut [u8]) -> Result<usize> {
                Ok(0)
            }
            fn write(&self, _n: NodeId, _o: u64, _b: &[u8]) -> Result<usize> {
                Ok(0)
            }
            fn enumerate(&self, _d: NodeId) -> Result<Vec<Entry>> {
                Ok(vec![])
            }
            fn attrs(&self, _n: NodeId) -> Result<Attrs> {
                Ok(Attrs {
                    node: NodeId::ROOT,
                    kind: NodeKind::Directory,
                    size: 0,
                    unix_mode: 0o040755,
                    nlink: 1,
                    mtime: SystemTime::UNIX_EPOCH,
                })
            }
            fn invalidate(&self, _n: NodeId) -> Result<()> {
                Ok(())
            }
            fn flush(&self, _n: NodeId) -> Result<()> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }
        let s = CountFlush(AtomicUsize::new(0));
        s.release(NodeId::ROOT).unwrap();
        assert_eq!(s.0.load(Ordering::SeqCst), 1);
    }
}

// ---------------------------------------------------------------------------
// Capture-after-write-op coverage.
//
// The materialize pass in `core::capture` has distinct branches for
// pending file overrides, symlink overrides, dir tombstones, explicit
// empty dirs, and the captured-counterpart-load path. The unit-level
// write-op tests in `mod write_ops` exercise the pre-capture state;
// these add coverage for what the captured tree actually looks like
// once those overlay actions flow through capture.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod capture_write_ops {
    use std::path::Path;

    use objects::object::FileMode;

    use super::*;

    fn dump_tree(store: &dyn ObjectStore, hash: &ContentHash) -> Tree {
        store.get_tree(hash).unwrap().unwrap()
    }

    /// `make_dir` followed by `capture` must materialize the empty
    /// directory as a zero-entry tree under the parent (the
    /// `force_empty` arm of `materialize`).
    #[test]
    fn capture_after_make_dir_materializes_empty_subtree() {
        let (_temp, mount) = open_mount();
        mount.make_dir(NodeId::ROOT, OsStr::new("blank")).unwrap();
        let id = mount.capture(Some("empty dir".into())).unwrap();
        let store = mount.repo_handle().store();
        let state = store.get_state(&id).unwrap().unwrap();
        let root = dump_tree(store, &state.tree);
        let blank = root.get("blank").expect("blank dir survives capture");
        assert!(blank.is_tree());
        let blank_tree = dump_tree(store, &blank.hash);
        assert_eq!(blank_tree.entries().len(), 0);
    }

    /// `create_symlink` then `capture` writes the target as a CAS blob
    /// and emits a Symlink tree entry referring to it.
    #[test]
    fn capture_after_create_symlink_emits_symlink_entry() {
        let (_temp, mount) = open_mount();
        mount
            .create_symlink(
                NodeId::ROOT,
                OsStr::new("alias.txt"),
                Path::new("hello.txt"),
            )
            .unwrap();
        let id = mount.capture(Some("symlink".into())).unwrap();
        let store = mount.repo_handle().store();
        let state = store.get_state(&id).unwrap().unwrap();
        let root = dump_tree(store, &state.tree);
        let alias = root.get("alias.txt").expect("symlink lands in tree");
        assert!(matches!(alias.mode, objects::object::FileMode::Symlink));
    }

    /// A rmdir tombstone for a captured dir must drop the whole subtree
    /// from the next capture (the `dir_deletions` branch).
    #[test]
    fn capture_after_rmdir_drops_subtree() {
        // Build a fresh repo with a `dir/` containing one file; capture
        // it; remount; rmdir+capture should produce a root without `dir/`.
        let (_temp, mount) = fresh_mount();
        let node = create_pending_file(&mount, "dir/only.rs", FileMode::Normal);
        mount.write(node, 0, b"x").unwrap();
        mount.flush_all().unwrap();
        mount.capture(Some("plant".into())).unwrap();
        // Unlink the file then rmdir the now-empty dir.
        mount.unlink_path("dir/only.rs").unwrap();
        let id = mount.capture(Some("delete".into())).unwrap();
        let store = mount.repo_handle().store();
        let state = store.get_state(&id).unwrap().unwrap();
        let root = dump_tree(store, &state.tree);
        assert!(root.get("dir").is_none(), "dir/ should be pruned");
    }

    /// `rename_entry` across an overlay-only file followed by capture
    /// materializes the renamed name (and not the old name).
    #[test]
    fn capture_after_rename_overlay_file_uses_new_name() {
        let (_temp, mount) = open_mount();
        let src = mount
            .create_file(NodeId::ROOT, OsStr::new("draft"), FileMode::Normal, false)
            .unwrap();
        mount.write(src.node, 0, b"body").unwrap();
        mount.flush(src.node).unwrap();
        mount
            .rename_entry(
                NodeId::ROOT,
                OsStr::new("draft"),
                NodeId::ROOT,
                OsStr::new("final.txt"),
            )
            .unwrap();
        let id = mount.capture(Some("rename".into())).unwrap();
        let store = mount.repo_handle().store();
        let state = store.get_state(&id).unwrap().unwrap();
        let root = dump_tree(store, &state.tree);
        assert!(root.get("draft").is_none(), "old name must be gone");
        assert!(root.get("final.txt").is_some(), "new name must exist");
    }
}
