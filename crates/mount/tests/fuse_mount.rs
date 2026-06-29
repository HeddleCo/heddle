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
//!   mounted file. Locks in cached mode (heddle#87): with no
//!   `FOPEN_DIRECT_IO`, the page cache is the mapping substrate and
//!   shared mmap works on any FUSE-capable kernel — the path
//!   rust-analyzer, cargo, IDEs, and `grep --mmap` rely on. (No longer
//!   needs the `FUSE_DIRECT_IO_ALLOW_MMAP` cap or Linux 5.16+.)
//! * [`fuse_mount_cache_stays_coherent_after_write`] — heddle#87
//!   coherence red-commit. Write→close→reopen serves *fresh* content
//!   under cached mode, proving active inode invalidation closes the
//!   stale-read hazard that `FOPEN_DIRECT_IO` used to paper over.
//! * [`fuse_mount_serves_repeat_reads_from_cache`] — heddle#87
//!   cache-mode-active red-commit. A repeat read of unchanged content
//!   is served from the kernel page cache (the FUSE `read`-callback
//!   counter stays flat), proving the mount isn't bypassing the cache.
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

/// Like [`mount_fixture`], but also hands back the FUSE `read`-callback
/// counter so a test can observe whether the kernel page cache is
/// serving repeated reads (cached mode, heddle#87).
fn mount_fixture_with_read_counter(
    repo: Repository,
) -> (
    BackgroundSession,
    TempDir,
    std::sync::Arc<std::sync::atomic::AtomicU64>,
) {
    let mount = ContentAddressedMount::new(repo, "main").expect("open mount");
    let mountpoint = TempDir::new().expect("tempdir for mountpoint");
    let shell = FuseShell::new(mount);
    let read_calls = shell.read_calls_handle();
    let session = shell
        .mount_background(mountpoint.path())
        .expect("mount session");

    let target = mountpoint.path().join("hello.txt");
    wait_for(&target, true, Duration::from_secs(5));
    (session, mountpoint, read_calls)
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

/// **heddle#87 red-commit (coherence).** The write→close→reopen
/// stale-read hazard that heddle#74 r1 originally papered over with
/// `FOPEN_DIRECT_IO` must stay closed *under cached mode* — i.e. with
/// the page cache live and coherence maintained by active inode
/// invalidation instead.
///
/// The scenario is deliberately the worst case for a page cache:
///   1. Read the file through a fresh fd → kernel caches the bytes.
///   2. Truncate-and-rewrite with *different-length* content through a
///      *separate* fd, then close (→ `flush`/`release` promote the hot
///      tier and fire `inval_inode`).
///   3. Open a brand-new fd and read again.
///
/// Without invalidation, step 3 would hand back the bytes cached in
/// step 1 (the page cache is keyed off the kernel-side inode, which is
/// unchanged). With the heddle#87 invalidation contract, step 2's
/// `inval_inode(ino, 0, -1)` drops those pages, so step 3 re-asks the
/// shell and observes the fresh content. A regression that drops the
/// invalidation (or re-introduces a caching mode the inval can't reach)
/// fails here with the stale bytes.
#[test]
#[ignore = "requires FUSE on host; opt-in via --ignored"]
fn fuse_mount_cache_stays_coherent_after_write() {
    let (_repo_dir, repo) = build_fixture();
    let (session, mountpoint) = mount_fixture(repo);

    let target = mountpoint.path().join("hello.txt");

    // 1. Prime the kernel page cache with the captured content.
    assert_eq!(
        fs::read_to_string(&target).expect("prime read"),
        "world",
        "captured content visible before write"
    );

    // 2. Rewrite with longer, different content through a separate fd.
    //    `truncate(true)` exercises the `setattr(size=0)` → invalidation
    //    path as well as the `write` + `flush`/`release` path.
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&target)
            .expect("open for truncate+write");
        f.write_all(b"FRESH-AND-LONGER-CONTENT")
            .expect("write new content");
        // drop → close → flush + release → promote + inval_inode.
    }

    // 3. Fresh open + read. Must observe the new bytes, not the cached
    //    "world" from step 1.
    //
    //    Invalidation is dispatched off the FUSE worker thread (it has
    //    to be — a synchronous notify from inside the mutating callback
    //    deadlocks on the kernel inode lock), so it lands a beat after
    //    `close(2)` returns rather than synchronously with it. Poll a
    //    bounded window — deliberately *shorter than the 1 s attr TTL*
    //    (`fuse::TTL`) — for the fresh content. The short window is what
    //    makes this a real test of *active* invalidation rather than
    //    passive TTL expiry: a coherent mount drops the page cache and
    //    converges within single-digit ms, but a broken mount (no
    //    invalidation) can only self-heal once the kernel re-validates
    //    attrs at the TTL boundary — which is outside this window, so it
    //    would still be serving the stale "world" when we assert.
    let deadline = Instant::now() + Duration::from_millis(700);
    let mut after = fs::read_to_string(&target).expect("read after rewrite");
    while after != "FRESH-AND-LONGER-CONTENT" && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
        after = fs::read_to_string(&target).expect("re-read after rewrite");
    }
    assert_eq!(
        after, "FRESH-AND-LONGER-CONTENT",
        "active invalidation must drop the stale page cache so the \
         reopened fd sees fresh content (heddle#87 coherence)"
    );

    drop(session);
}

/// **heddle#87 red-commit (cache-mode-active).** Proves the kernel
/// page cache is actually serving repeated reads — i.e. the mount runs
/// in cached mode, not the old `FOPEN_DIRECT_IO` bypass.
///
/// We read the same unchanged file twice through fresh fds and watch
/// the FUSE `read`-callback counter. In cached mode the *second* whole
/// read is served entirely from the kernel page cache, so the counter
/// must not advance for it. In the old direct-IO mode every userspace
/// `read(2)` becomes a FUSE `read` callback, so the counter would climb
/// on the second read too — which is exactly the throughput cost
/// heddle#87 removes. A regression that re-introduces `FOPEN_DIRECT_IO`
/// fails here: the counter advances on the second read.
#[test]
#[ignore = "requires FUSE on host; opt-in via --ignored"]
fn fuse_mount_serves_repeat_reads_from_cache() {
    use std::sync::atomic::Ordering;

    let (_repo_dir, repo) = build_fixture();
    let (session, mountpoint, read_calls) = mount_fixture_with_read_counter(repo);

    let target = mountpoint.path().join("hello.txt");

    // First read: populates the page cache. This *does* go through the
    // FUSE `read` callback (cold cache).
    assert_eq!(fs::read_to_string(&target).expect("first read"), "world");
    let after_first = read_calls.load(Ordering::Relaxed);
    assert!(
        after_first >= 1,
        "the cold first read must reach the FUSE read callback at least once \
         (got {after_first})"
    );

    // Second read of the same unchanged file through a fresh fd. In
    // cached mode the kernel serves it from the page cache and never
    // calls us; the counter stays flat.
    assert_eq!(fs::read_to_string(&target).expect("second read"), "world");
    let after_second = read_calls.load(Ordering::Relaxed);

    assert_eq!(
        after_second, after_first,
        "cached mode must serve the second read from the kernel page cache \
         without a FUSE round-trip — counter went {after_first} → {after_second}; \
         a nonzero delta means the mount is still bypassing the cache \
         (FOPEN_DIRECT_IO regression, heddle#87)"
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
/// Post-heddle#87 the shell runs in the kernel's default *cached*
/// mode — `open` no longer returns `FOPEN_DIRECT_IO` — so shared
/// `mmap` works out of the box: the page cache is the mapping
/// substrate, and we keep it live. This is what makes rust-analyzer,
/// cargo, IDEs, and `grep --mmap` work on heddle-mounted repos.
///
/// (Before heddle#87 this test had to opt the kernel into
/// `FUSE_DIRECT_IO_ALLOW_MMAP` to map a `FOPEN_DIRECT_IO` fd, which
/// required Linux 5.16+. Active invalidation removed both the
/// direct-IO flag and that kernel-version floor, so the test no
/// longer probes the kernel version.)
#[test]
#[ignore = "requires FUSE on host; opt-in via --ignored"]
fn fuse_mount_serves_mmap_readers() {
    let (_repo_dir, repo) = build_fixture();
    let (session, mountpoint) = mount_fixture(repo);

    let target = mountpoint.path().join("hello.txt");
    let file = fs::File::open(&target).expect("open mounted file");

    // SAFETY: `Mmap::map` is unsafe because the kernel may revoke the
    // mapping out from under us if another process truncates the
    // backing file. In this test we hold the only handle to the
    // mounted file for the lifetime of `mapping`, and we don't mutate
    // it through the mount. Soundness is on us; we accept the contract.
    let mapping = unsafe { memmap2::Mmap::map(&file) }
        .expect("mmap(MAP_SHARED) on mounted file (cached mode must be in effect)");

    assert_eq!(
        &mapping[..],
        b"world",
        "mmap'd bytes must match captured content"
    );

    drop(mapping);
    drop(file);
    drop(session);
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
