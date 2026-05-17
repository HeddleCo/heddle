// SPDX-License-Identifier: Apache-2.0
//! Windows ProjFS end-to-end mount tests.
//!
//! Mirrors `tests/fuse_mount.rs` for the Windows daily-use path.
//! Each test is `#[ignore]` and additionally gated on the
//! `HEDDLE_PROJFS_AVAILABLE=1` env var so contributors running
//! `cargo test -p heddle-mount` on macOS/Linux — or on a Windows
//! host without the optional feature — don't see spurious failures.
//!
//! To run locally:
//!
//! ```powershell
//! $env:HEDDLE_PROJFS_AVAILABLE="1"
//! cargo test -p heddle-mount --features projfs --test projfs_smoke -- --ignored
//! ```
//!
//! Enabling the optional feature once per host (admin PowerShell):
//!
//! ```powershell
//! Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS
//! ```
//!
//! See `crates/mount/README.md` for the full set of Windows install
//! notes. The CI matrix entry `projfs-smoke` in
//! `.github/workflows/rust-tests.yml` runs the same `-- --ignored`
//! invocation on `windows-latest` with the feature enabled.
//!
//! ## What each test locks in
//!
//! * [`projfs_mount_serves_blob_content`] — load-bearing daily-use
//!   smoke. Snapshot a file → mount → poll until the projection
//!   surfaces it → read via `std::fs::read_to_string` → drop session.
//! * [`projfs_mount_round_trips_writes_to_existing_file`] — write
//!   path. ProjFS doesn't deliver per-write callbacks, so the
//!   shell synthesises a `write+flush` on
//!   `PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_MODIFIED`. This test
//!   asserts the bridge actually promotes the edit back into the
//!   content-addressed store.
//! * [`projfs_mount_serves_concurrent_readers`] — N threads reading
//!   the same projected file. Surfaces obvious lock-ordering or
//!   placeholder-hydration race bugs.
//! * [`projfs_mount_unmounts_cleanly_on_session_drop`] — drop
//!   semantics. After the session is dropped the placeholder must
//!   no longer surface the captured file (or, at minimum, the
//!   projection's hydration callbacks stop firing — the now-stale
//!   NTFS placeholder can linger, but no new placeholders should
//!   appear).
//! * [`projfs_mount_hides_instance_id_sidecar_from_listing`] —
//!   regression catch for the heddle#54 leakage where the per-mount
//!   GUID lived at `<root>/.heddle_projfs_id` and showed up in
//!   `dir`. The sidecar is now stored in the *parent* directory by
//!   default (with an in-root fallback when the parent ACL refuses
//!   writes — see `crates/mount/src/projfs.rs::load_or_create_instance_id`);
//!   the projection's listing must contain only the captured files
//!   in the common case the smoke test runs in (writable temp parent).

#![cfg(all(target_os = "windows", feature = "projfs"))]

use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use mount::{ContentAddressedMount, ProjFsSession, ProjFsShell};
use repo::Repository;
use tempfile::TempDir;

/// Common skip-or-fail probe at the top of every test. Returns
/// `true` when the test should run; `false` when it should silently
/// no-op. Treats absence of `HEDDLE_PROJFS_AVAILABLE=1` as "this
/// host opted out" (the CI matrix sets it; a generic dev box does
/// not). Treats absence of `ProjectedFSLib.dll` as a soft skip with
/// an eprintln so the CI log shows the reason.
fn projfs_or_skip() -> bool {
    if env::var("HEDDLE_PROJFS_AVAILABLE").as_deref() != Ok("1") {
        eprintln!(
            "skipping: set HEDDLE_PROJFS_AVAILABLE=1 to opt this host \
             into the ProjFS smoke tests"
        );
        return false;
    }
    if !ProjFsShell::is_runtime_available() {
        eprintln!(
            "skipping: ProjFS runtime not available \
             (run `Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS` \
             from an admin PowerShell)"
        );
        return false;
    }
    true
}

/// Build a tiny repo with one captured file (`hello.txt` containing
/// `"world"`) and return the temp dir + an open `Repository` handle.
/// Mirrors `fuse_mount.rs::build_fixture`; both shipped because the
/// temp dir's `Drop` cleans up the on-disk repo and we don't want
/// it to fire before the test ends.
fn build_fixture() -> (TempDir, Repository) {
    let repo_dir = TempDir::new().expect("tempdir for repo");
    let repo = Repository::init_default(repo_dir.path()).expect("init_default");
    fs::write(repo_dir.path().join("hello.txt"), b"world").expect("write hello.txt");
    repo.snapshot(Some("fixture".into()), None)
        .expect("snapshot fixture");
    (repo_dir, repo)
}

/// Mount the fixture via ProjFS and return the session + an empty
/// tempdir for the mountpoint. Polls for the placeholder to surface
/// so callers can read immediately on return — ProjFS hydrates
/// placeholders lazily on access, but `mount_background` returns
/// before the first enumeration runs, so a kernel race could leave
/// `target.exists()` false for ~tens of milliseconds.
fn mount_fixture(repo: Repository) -> (ProjFsSession, TempDir) {
    let mount = ContentAddressedMount::new(repo, "main").expect("open mount");
    let mountpoint = TempDir::new().expect("tempdir for mountpoint");
    let session = ProjFsShell::new(mount)
        .mount_background(mountpoint.path())
        .expect("projfs mount_background");

    let target = mountpoint.path().join("hello.txt");
    wait_for(&target, true, Duration::from_secs(5));
    (session, mountpoint)
}

/// Poll up to `dur` for `target` to (dis)appear.
fn wait_for(target: &Path, expect_present: bool, dur: Duration) {
    let deadline = Instant::now() + dur;
    while target.exists() != expect_present && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "requires Windows + ProjFS; opt-in via HEDDLE_PROJFS_AVAILABLE=1"]
fn projfs_mount_serves_blob_content() {
    if !projfs_or_skip() {
        return;
    }
    let (_repo_dir, repo) = build_fixture();
    let (session, mountpoint) = mount_fixture(repo);

    let target = mountpoint.path().join("hello.txt");
    let read = fs::read_to_string(&target).expect("read mounted file");
    assert_eq!(read, "world");

    drop(session);
}

#[test]
#[ignore = "requires Windows + ProjFS; opt-in via HEDDLE_PROJFS_AVAILABLE=1"]
fn projfs_mount_round_trips_writes_to_existing_file() {
    if !projfs_or_skip() {
        return;
    }
    let (_repo_dir, repo) = build_fixture();
    let (session, mountpoint) = mount_fixture(repo);

    let target = mountpoint.path().join("hello.txt");
    assert_eq!(fs::read_to_string(&target).expect("read captured"), "world");

    // Open-for-write without `truncate(true)` — new content is the
    // same length as the original so we don't depend on a
    // setattr(size=0) hook (ProjFS routes truncation through the
    // standard NTFS handle, and the close-modified notification picks
    // up the final size; we don't have a separate path for it).
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .open(&target)
            .expect("open for write");
        f.write_all(b"WORLD").expect("write through mount");
        // Drop closes the file; the kernel fires
        // PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_MODIFIED, which
        // the shell's notification_trampoline turns into a single
        // write(node, 0, full)+flush against ContentAddressedMount.
    }

    // Re-read through the mount. The promoted pending-tier blob
    // shadows the captured snapshot, so we should see WORLD not
    // world. Give the kernel a beat for the close-notification to
    // round-trip back through the shell.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut after = String::new();
    while Instant::now() < deadline {
        after = fs::read_to_string(&target).expect("read after write");
        if after == "WORLD" {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        after, "WORLD",
        "expected write-through-mount to be visible on re-read",
    );

    drop(session);
}

#[test]
#[ignore = "requires Windows + ProjFS; opt-in via HEDDLE_PROJFS_AVAILABLE=1"]
fn projfs_mount_serves_concurrent_readers() {
    if !projfs_or_skip() {
        return;
    }
    let (_repo_dir, repo) = build_fixture();
    let (session, mountpoint) = mount_fixture(repo);

    let target = Arc::new(mountpoint.path().join("hello.txt"));

    // Four threads × twenty reads each. Surfaces lock-ordering or
    // placeholder-hydration races between the ProjFS worker threads
    // and the shell's read dispatch. Same shape as the FUSE smoke
    // test so cross-platform expectations match.
    const THREADS: usize = 4;
    const READS_PER_THREAD: usize = 20;

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let target = Arc::clone(&target);
            thread::spawn(move || {
                for _ in 0..READS_PER_THREAD {
                    let read = fs::read_to_string(&*target).expect("concurrent read");
                    assert_eq!(read, "world");
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("reader thread panicked");
    }

    drop(session);
}

#[test]
#[ignore = "requires Windows + ProjFS; opt-in via HEDDLE_PROJFS_AVAILABLE=1"]
fn projfs_mount_unmounts_cleanly_on_session_drop() {
    if !projfs_or_skip() {
        return;
    }
    let (_repo_dir, repo) = build_fixture();
    let (session, mountpoint) = mount_fixture(repo);
    let target = mountpoint.path().join("hello.txt");

    assert!(
        target.exists(),
        "fixture file must be visible before drop",
    );

    let mp: PathBuf = mountpoint.path().to_path_buf();
    drop(session);

    // ProjFS leaves hydrated placeholders on disk after
    // `PrjStopVirtualizing` — they become regular NTFS files at
    // their last-hydrated contents. That's the documented contract
    // (offline-edit story: a stopped projection's files are still
    // readable; reattaching with the same GUID re-projects deltas).
    //
    // So we don't assert `!target.exists()` — what we *do* assert
    // is that the session reclaims its kernel handle cleanly (no
    // panic on the explicit drop above) and that re-touching the
    // file does not trigger any more shell callbacks. The smoke
    // for "no more callbacks" is indirect: if a callback re-fired
    // post-drop, we'd dereference a freed `InstanceContext` and
    // segfault the test process. Surviving the drop is the assert.

    // Probe the path once after a small settle: the file may or
    // may not be present depending on whether the kernel already
    // hydrated it. Either is correct.
    wait_for(&target, false, Duration::from_millis(200));
    let _ = fs::read_dir(&mp); // doesn't panic
}

/// Windows regression test for heddle#105. `Repository::init_default`
/// used to fail with `PermissionDenied` on any Windows tempdir because
/// `objects::fs_atomic::write_file_atomic` called `sync_directory` on
/// `.heddle/oplog`, and `sync_directory` opened the directory with
/// `OpenOptions::new().read(true)` + `sync_all()` — neither operation
/// is meaningful on Windows (directory handles require
/// `FILE_FLAG_BACKUP_SEMANTICS` to open, and `FlushFileBuffers` on a
/// directory handle returns `ERROR_ACCESS_DENIED`).
///
/// The bug was invisible until heddle#102 fixed the silent-red Windows
/// CI job and the ProjFS smoke tests in this file started actually
/// running — at which point every fixture builder hit it on the
/// `init_default` call in [`build_fixture`].
///
/// This test exercises only the init path (no ProjFS mount) so the
/// regression stays caught even if a later refactor makes the broader
/// smoke tests skip earlier. Kept here (rather than in `heddle-repo`'s
/// own test suite) because `projfs-smoke` is the workflow's only
/// Windows job — moving it would re-hide the regression on Linux-only
/// runs.
#[test]
#[ignore = "Windows regression for heddle#105; opt-in alongside projfs-smoke"]
fn init_default_on_windows_tempdir_does_not_permission_deny() {
    let dir = TempDir::new().expect("tempdir for repo");
    let result = Repository::init_default(dir.path());
    if let Err(e) = &result {
        let msg = e.to_string().to_lowercase();
        assert!(
            !msg.contains("permission denied"),
            "init_default regressed to PermissionDenied on a writable Windows \
             tempdir (heddle#105): {e}"
        );
    }
    result.expect("init_default on Windows tempdir");
}

#[test]
#[ignore = "requires Windows + ProjFS; opt-in via HEDDLE_PROJFS_AVAILABLE=1"]
fn projfs_mount_hides_instance_id_sidecar_from_listing() {
    if !projfs_or_skip() {
        return;
    }
    let (_repo_dir, repo) = build_fixture();
    let (session, mountpoint) = mount_fixture(repo);

    // Listing the mountpoint should surface only files captured in
    // the snapshot (`hello.txt`) — not the `.heddle-projfs-id`
    // sidecar that lives in the *parent* directory of the
    // virtualization root.
    //
    // Pre-fix the sidecar lived at `<root>/.heddle_projfs_id` and
    // showed up in `dir` next to the projected content. The
    // production blocker was UX (users `cd`ing into a mount saw
    // a stray metadata file) but the deeper issue was inconsistency
    // between NTFS-side and ProjFS-side enumeration: the NTFS list
    // included the sidecar, the projection callback did not, and
    // tools that hit the two paths got mismatched views.
    //
    // The smoke test asserts the *default* behaviour: when the
    // parent directory is writable (always true under `TempDir`),
    // the sidecar lands in the parent and the mountpoint listing
    // stays clean. The in-root fallback used for restricted-parent
    // mounts is exercised by the unit test
    // `fallback_instance_id_sidecar_lives_inside_the_virtualization_root`
    // in `src/projfs.rs`.
    let mut names: Vec<String> = fs::read_dir(mountpoint.path())
        .expect("read mountpoint dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();

    assert!(
        !names.iter().any(|n| n.contains("heddle-projfs-id") || n.contains("heddle_projfs_id")),
        "instance-ID sidecar must not appear in mounted listing: {names:?}",
    );
    assert!(
        names.iter().any(|n| n == "hello.txt"),
        "captured file must appear in mounted listing: {names:?}",
    );

    drop(session);
}
