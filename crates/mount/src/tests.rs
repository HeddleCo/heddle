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
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use objects::{
    error::HeddleError,
    object::{Action, ActionId, Blob, ChangeId, ContentHash, State, Tree},
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
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::UNIX_EPOCH,
    };

    use crate::{
        error::{MountError, Result},
        shell::{Attrs, DIR_UNIX_MODE, Entry, NodeId, NodeKind, PlatformShell},
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
    let main_id = repo_a.refs().get_thread("main").unwrap().unwrap();
    repo_a.refs().set_thread("feature", &main_id).unwrap();
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
    let head = repo_check.refs().get_thread("main").unwrap().unwrap();
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
    let base_state_id = repo.refs().get_thread("main").unwrap().unwrap();
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
    let main_id = repo.refs().get_thread("main").unwrap().unwrap();
    // Make 9 sibling threads.
    for i in 0..9 {
        repo.refs()
            .set_thread(&format!("feat-{i}"), &main_id)
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
