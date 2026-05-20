// SPDX-License-Identifier: Apache-2.0
//! Linux end-to-end mount tests.
//!
//! Marked `#[ignore]` so the default `cargo test` invocation doesn't
//! auto-run them: FUSE needs a working `fuse` kernel module + the
//! `fusermount3` binary on PATH, and that isn't a fair assumption for
//! a generic CI runner. CI opts in explicitly via the `fuse-smoke`
//! matrix entry in `.github/workflows/rust-tests.yml`, which installs
//! `fuse3 libfuse3-dev` first and then runs:
//!
//! ```bash
//! cargo test -p heddle-mount --features fuse --test fuse_mount -- --ignored
//! ```
//!
//! Local invocation matches CI. If a test wedges, `fusermount3 -u
//! <mountpoint>` from outside (or rebooting the runner) clears the
//! kernel side; the per-test `TempDir` will be reaped by the OS even
//! if the mount lingered.
//!
//! ## What each test locks in
//!
//! * [`fuse_mount_serves_blob_content`] — the load-bearing daily-use
//!   smoke. Snapshot a file → mount → read via `std::fs::read_to_string`
//!   → assert exact content → drop session.
//! * [`fuse_mount_round_trips_writes_to_existing_file`] — write path.
//!   Captures a file, mounts, opens-for-write, writes new bytes,
//!   closes, re-reads through the mount, asserts the hot/warm tier
//!   served the new content (not the captured original).
//! * [`fuse_mount_serves_concurrent_readers`] — N threads reading the
//!   same file repeatedly. Locks in that the FUSE worker dispatching
//!   to `ContentAddressedMount` is safe under realistic read
//!   parallelism (a build of any non-trivial project will issue tens
//!   of these per second).
//! * [`fuse_mount_serves_mmap_readers`] — `mmap(MAP_SHARED, ...)` on a
//!   mounted file. Locks in the [`InitFlags::FUSE_DIRECT_IO_ALLOW_MMAP`]
//!   opt-in: without it, every `open` reply carrying
//!   `FOPEN_DIRECT_IO` forces `mmap(MAP_SHARED, ...)` to fail with
//!   `ENODEV`, which breaks rust-analyzer, cargo, IDEs, and
//!   `grep --mmap` on heddle-mounted trees. Requires Linux 5.16+;
//!   skips itself on older kernels (the cap is silently dropped).
//! * [`fuse_mount_unmounts_cleanly_on_session_drop`] — drop semantics.
//!   After the session is dropped the mountpoint must no longer
//!   serve the captured file; reading should fail with `NotFound` and
//!   the directory listing must be empty (the underlying tempdir).
//!
//! Each test reuses [`build_fixture`] to construct a deterministic
//! repo and gives the kernel a bounded poll window before asserting
//! visibility — `mount_background` returns once the session is
//! spawned, but the FS isn't visible until the kernel finishes
//! attaching it (usually <100 ms).

#![cfg(all(target_os = "linux", feature = "fuse"))]

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use mount::{BackgroundSession, ContentAddressedMount, FuseShell};
use repo::Repository;
use tempfile::TempDir;

/// Build a tiny repo with one captured file (`hello.txt` containing
/// `"world"`) and return the temp dir + an open `Repository` handle.
/// Both shipped because the temp dir's `Drop` cleans up the on-disk
/// repo and we don't want it to fire before the test ends.
fn build_fixture() -> (TempDir, Repository) {
    let repo_dir = TempDir::new().expect("tempdir for repo");
    let repo = Repository::init_default(repo_dir.path()).expect("init_default");
    fs::write(repo_dir.path().join("hello.txt"), b"world").expect("write hello.txt");
    repo.snapshot(Some("fixture".into()), None)
        .expect("snapshot fixture");
    (repo_dir, repo)
}

/// Mount the fixture via FUSE and return the session + an empty
/// tempdir for the mountpoint. Polls for the kernel to attach the
/// FS so callers can read immediately on return.
fn mount_fixture(repo: Repository) -> (BackgroundSession, TempDir) {
    let mount = ContentAddressedMount::new(repo, "main").expect("open mount");
    let mountpoint = TempDir::new().expect("tempdir for mountpoint");
    let session = FuseShell::new(mount)
        .mount_background(mountpoint.path())
        .expect("mount session");

    // Wait briefly for the FUSE worker to be ready. `mount_background`
    // returns once the session is spawned, but the kernel may take a
    // moment to publish the FS.
    let target = mountpoint.path().join("hello.txt");
    wait_for(&target, true, Duration::from_secs(5));
    (session, mountpoint)
}

/// Poll up to `deadline` for `target` to exist (`expect_present=true`)
/// or to no longer exist (`expect_present=false`).
fn wait_for(target: &Path, expect_present: bool, dur: Duration) {
    let deadline = Instant::now() + dur;
    while target.exists() != expect_present && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "requires FUSE on host; opt-in via --ignored"]
fn fuse_mount_serves_blob_content() {
    let (_repo_dir, repo) = build_fixture();
    let (session, mountpoint) = mount_fixture(repo);

    let target = mountpoint.path().join("hello.txt");
    let read = fs::read_to_string(&target).expect("read mounted file");
    assert_eq!(read, "world");

    drop(session); // triggers unmount
}

#[test]
#[ignore = "requires FUSE on host; opt-in via --ignored"]
fn fuse_mount_round_trips_writes_to_existing_file() {
    let (_repo_dir, repo) = build_fixture();
    let (session, mountpoint) = mount_fixture(repo);

    let target = mountpoint.path().join("hello.txt");

    // Sanity: captured content is visible.
    assert_eq!(fs::read_to_string(&target).expect("read captured"), "world");

    // Open-for-write *without* `truncate(true)` so we don't depend on
    // a `setattr(size=0)` callback the FUSE shell doesn't implement
    // yet (the kernel issues setattr-with-size for `O_TRUNC` and the
    // default fuser impl is `ENOSYS`). New content is the same
    // length as the original so no truncation is needed for the
    // read-back to be unambiguous.
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .open(&target)
            .expect("open for write");
        f.write_all(b"WORLD").expect("write through mount");
        // Drop closes the file → kernel issues `flush` then `release`,
        // which the shell promotes through the hot/warm tier.
    }

    // Re-read through the mount. The pending tier (hot buffer or
    // promoted warm blob) must shadow the captured state's blob.
    let after = fs::read_to_string(&target).expect("read after write");
    assert_eq!(
        after, "WORLD",
        "expected write-through-mount to be visible on re-read"
    );

    drop(session);
}

#[test]
#[ignore = "requires FUSE on host; opt-in via --ignored"]
fn fuse_mount_serves_concurrent_readers() {
    let (_repo_dir, repo) = build_fixture();
    let (session, mountpoint) = mount_fixture(repo);

    let target = Arc::new(mountpoint.path().join("hello.txt"));

    // Four threads × twenty reads each = 80 round-trips through the
    // FUSE worker. This is enough to surface obvious lock-ordering
    // or aliasing bugs in the shell's read dispatch; it's deliberately
    // small enough to stay fast in CI.
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

/// `mmap(MAP_SHARED, ...)` against a mounted file must succeed and
/// must return the captured bytes when the mapping is read.
///
/// This locks in `FUSE_DIRECT_IO_ALLOW_MMAP`: the shell unconditionally
/// returns `FOPEN_DIRECT_IO` from `open` so kernel page-cache reads
/// don't shadow hot-tier writes, and under default kernel semantics
/// that disables shared `mmap` on every fd (the page cache is the
/// mapping substrate; bypass it and the kernel refuses to map the
/// file, returning `ENODEV` from the `mmap` syscall). The shell opts
/// out of that restriction via `InitFlags::FUSE_DIRECT_IO_ALLOW_MMAP`
/// in its `init` callback — without that opt-in, this test fails
/// with `Errno::NODEV` at the `Mmap::map(&file)` call, which is the
/// exact failure mode rust-analyzer, cargo, and IDEs hit on
/// heddle-mounted repos.
///
/// The cap requires Linux 5.16+ (when the kernel-side flag was
/// added). Older kernels silently drop the request — fuser logs it at
/// debug — and this test will fail there. We probe the kernel version
/// up front and skip rather than fail on older kernels so the
/// `fuse-smoke` CI matrix stays portable.
#[test]
#[ignore = "requires FUSE on host (Linux 5.16+); opt-in via --ignored"]
fn fuse_mount_serves_mmap_readers() {
    if !kernel_at_least(5, 16) {
        eprintln!(
            "skipping fuse_mount_serves_mmap_readers: \
             FUSE_DIRECT_IO_ALLOW_MMAP requires Linux 5.16+"
        );
        return;
    }

    let (_repo_dir, repo) = build_fixture();
    let (session, mountpoint) = mount_fixture(repo);

    let target = mountpoint.path().join("hello.txt");
    let file = fs::File::open(&target).expect("open mounted file");

    // SAFETY: `Mmap::map` is unsafe because the kernel may revoke the
    // mapping out from under us if another process truncates the
    // backing file. In this test we hold the only handle to the
    // mounted file for the lifetime of `mapping`, and the FUSE shell
    // doesn't expose `setattr(size)` (so truncation is impossible
    // through the mount). Soundness is on us; we accept the contract.
    let mapping = unsafe { memmap2::Mmap::map(&file) }
        .expect("mmap(MAP_SHARED) on mounted file (FUSE_DIRECT_IO_ALLOW_MMAP must be enabled)");

    assert_eq!(
        &mapping[..],
        b"world",
        "mmap'd bytes must match captured content"
    );

    drop(mapping);
    drop(file);
    drop(session);
}

/// Parse `/proc/sys/kernel/osrelease` to skip the mmap test on
/// kernels older than `(major, minor)`. The cap was added in 5.16; a
/// `mount.fuse.kernel-old.smoke` CI runner on an older kernel should
/// skip rather than fail.
fn kernel_at_least(major: u32, minor: u32) -> bool {
    let raw = match fs::read_to_string("/proc/sys/kernel/osrelease") {
        Ok(s) => s,
        Err(_) => return true, // not Linux-shaped — let the test attempt and report
    };
    let version_str = raw.trim().split('-').next().unwrap_or("");
    let mut parts = version_str.split('.').filter_map(|s| s.parse::<u32>().ok());
    let host_major = parts.next().unwrap_or(0);
    let host_minor = parts.next().unwrap_or(0);
    (host_major, host_minor) >= (major, minor)
}

/// End-to-end verification of the write-side overlay ops added in
/// heddle#180. Each kernel op the issue called out (`create`,
/// `mkdir`, `unlink`, `rmdir`, `rename`, `setattr`/truncate,
/// `symlink`) is exercised in sequence against a single mounted
/// fixture, and the resulting state is read back through the same
/// mount to confirm overlay visibility. This is the FUSE-layer
/// analogue of the `tests::write_ops::*` unit tests — same
/// behavior, but driven through the real kernel-userspace round
/// trip so a regression in the trampoline / errno mapping / direct-
/// io flag is caught even when the core remains correct.
#[test]
#[ignore = "requires FUSE on host; opt-in via --ignored"]
fn fuse_mount_round_trips_write_side_ops() {
    let (_repo_dir, repo) = build_fixture();
    let (session, mountpoint) = mount_fixture(repo);
    let root = mountpoint.path();

    // create + write — the cargo `open(O_CREAT)` path. Before the
    // shell wired this up, the very first cargo invocation hit
    // ENOSYS on `Cargo.lock`.
    let lock_path = root.join("Cargo.lock");
    {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&lock_path)
            .expect("create + open Cargo.lock");
        f.write_all(b"[package]\nname=\"x\"\n").expect("write");
    }
    let read_back = fs::read_to_string(&lock_path).expect("read created file");
    assert_eq!(read_back, "[package]\nname=\"x\"\n");

    // O_CREAT|O_EXCL against the same path must fail with EEXIST.
    let excl_err = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&lock_path)
        .expect_err("O_CREAT|O_EXCL on existing must fail");
    assert_eq!(
        excl_err.kind(),
        std::io::ErrorKind::AlreadyExists,
        "exclusive create surfaced wrong errno: {excl_err:?}"
    );

    // mkdir + nested create.
    let target_dir = root.join("target");
    fs::create_dir(&target_dir).expect("mkdir target/");
    assert!(target_dir.is_dir(), "mkdir didn't make a directory");
    let nested_file = target_dir.join("output.bin");
    fs::write(&nested_file, b"build artifact").expect("write under new dir");
    assert_eq!(fs::read(&nested_file).unwrap(), b"build artifact");

    // rename — cargo's `.tmp → atomic-final` shape.
    let tmp = root.join("hello.txt.tmp");
    fs::write(&tmp, b"NEW\n").expect("write tmp");
    fs::rename(&tmp, root.join("hello.txt")).expect("rename over existing");
    assert!(!tmp.exists(), "tmp source still visible after rename");
    let after_rename = fs::read_to_string(root.join("hello.txt")).expect("read renamed");
    assert_eq!(after_rename, "NEW\n");

    // setattr (truncate) — O_TRUNC against an existing overlay file
    // (the just-renamed hello.txt). The kernel issues
    // `setattr(size=0)` before the first write, which must clear the
    // buffer; otherwise the next write would tack onto the existing
    // bytes.
    let trunc_path = root.join("hello.txt");
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&trunc_path)
            .expect("open O_TRUNC");
        f.write_all(b"TRUNCATED").expect("write after truncate");
    }
    assert_eq!(
        fs::read_to_string(&trunc_path).unwrap(),
        "TRUNCATED",
        "O_TRUNC didn't clear the prior content"
    );

    // symlink + readlink.
    let link_path = root.join("hello.lnk");
    std::os::unix::fs::symlink("hello.txt", &link_path).expect("symlink");
    let resolved = fs::read_link(&link_path).expect("readlink");
    assert_eq!(resolved.as_os_str(), "hello.txt");

    // unlink — drop a captured file through the mount. Cargo.lock
    // is the file we created at the top of the test; the captured
    // hello.txt was already replaced via rename, so unlinking it now
    // would only touch the overlay-only entry. Unlinking the freshly
    // created Cargo.lock exercises the warm-tier-only deletion path.
    fs::remove_file(root.join("Cargo.lock")).expect("unlink");
    assert!(
        !root.join("Cargo.lock").exists(),
        "unlinked file still visible to the mount"
    );

    // rmdir empty pending dir (target/ had output.bin, so unlink it
    // first to make rmdir succeed).
    fs::remove_file(&nested_file).expect("unlink before rmdir");
    fs::remove_dir(&target_dir).expect("rmdir of empty pending dir");
    assert!(!target_dir.exists(), "rmdir didn't remove the directory");

    drop(session);
}

#[test]
#[ignore = "requires FUSE on host; opt-in via --ignored"]
fn fuse_mount_unmounts_cleanly_on_session_drop() {
    let (_repo_dir, repo) = build_fixture();
    let (session, mountpoint) = mount_fixture(repo);
    let target = mountpoint.path().join("hello.txt");

    // Pre-condition: file visible.
    assert!(target.exists(), "fixture file must be visible before drop");

    // Capture the mountpoint path before dropping the session so we
    // can probe it after.
    let mp: PathBuf = mountpoint.path().to_path_buf();
    drop(session);

    // Give the kernel a beat to tear the mount down. fuser's
    // `BackgroundSession::Drop` signals the unmount synchronously,
    // but the kernel may publish the namespace change a tick later.
    wait_for(&target, false, Duration::from_secs(5));

    // The captured file must no longer be visible: either the
    // mountpoint resolves to an empty backing directory, or the
    // dentry has gone stale and `exists()` is false.
    assert!(
        !target.exists(),
        "hello.txt must disappear from {} after unmount",
        mp.display()
    );

    // And the mountpoint directory listing must be empty (the
    // underlying tempdir starts empty and the mount didn't write
    // into the backing FS).
    let listing: Vec<_> = fs::read_dir(&mp)
        .expect("read mountpoint dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .collect();
    assert!(
        listing.is_empty(),
        "mountpoint not empty after unmount: {listing:?}"
    );
}
