// SPDX-License-Identifier: Apache-2.0
//! Linux end-to-end mount test.
//!
//! Marked `#[ignore]` so CI doesn't auto-run it: FUSE needs a working
//! `fuse` kernel module + the `fusermount3` binary on PATH, and that
//! isn't a fair assumption for a generic CI runner. To run:
//!
//! ```bash
//! cargo test -p mount --features fuse --test fuse_mount -- --ignored
//! ```

#![cfg(all(target_os = "linux", feature = "fuse"))]

use std::{
    fs,
    time::{Duration, Instant},
};

use mount::{ContentAddressedMount, FuseShell};
use repo::Repository;
use tempfile::TempDir;

#[test]
#[ignore = "requires FUSE on host; opt-in via --ignored"]
fn fuse_mount_serves_blob_content() {
    let repo_dir = TempDir::new().unwrap();
    let repo = Repository::init_default(repo_dir.path()).unwrap();
    fs::write(repo_dir.path().join("hello.txt"), b"world").unwrap();
    repo.snapshot(Some("fixture".into()), None).unwrap();

    let mount = ContentAddressedMount::new(repo, "main").unwrap();
    let mountpoint = TempDir::new().unwrap();
    let session = FuseShell::new(mount)
        .mount_background(mountpoint.path())
        .expect("mount session");

    // Wait briefly for the FUSE worker to be ready. `mount_background`
    // returns once the session is spawned, but the kernel may take a
    // moment to publish the FS. A short bounded poll is plenty.
    let target = mountpoint.path().join("hello.txt");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !target.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }

    let read = fs::read_to_string(&target).expect("read mounted file");
    assert_eq!(read, "world");

    drop(session); // triggers unmount
}