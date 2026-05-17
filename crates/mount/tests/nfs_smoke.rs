// SPDX-License-Identifier: Apache-2.0
//! NFS fallback end-to-end mount test.
//!
//! Marked `#[ignore]` and additionally gated on the
//! `HEDDLE_NFS_AVAILABLE=1` env var so CI only opts in on
//! machines where the test can plausibly succeed:
//!
//!   * Linux: must have CAP_SYS_ADMIN or `sudo -n mount` working.
//!   * macOS: must have admin sudo and `mount_nfs` available
//!     (default on macOS).
//!   * Windows: must have the "Services for NFS — Client for NFS"
//!     optional feature installed and an elevated console.
//!
//! To run locally on macOS / Linux:
//!
//! ```bash
//! HEDDLE_NFS_AVAILABLE=1 sudo -E \
//!   cargo test -p heddle-mount --features nfs --test nfs_smoke -- --ignored
//! ```
//!
//! The fallback path in `crates/cli/src/cli/commands/mount_lifecycle.rs`
//! exercises this same shell as a runtime fallback when the host's
//! native adapter (FUSE / FSKit / ProjFS) is unavailable.

#![cfg(feature = "nfs")]

use std::{
    env,
    time::{Duration, Instant},
};

use mount::{ContentAddressedMount, NfsShell};
use repo::Repository;
use tempfile::TempDir;

#[test]
#[ignore = "requires sudo / admin to mount NFS; opt-in via HEDDLE_NFS_AVAILABLE=1"]
fn nfs_mount_serves_blob_content() {
    if env::var("HEDDLE_NFS_AVAILABLE").as_deref() != Ok("1") {
        eprintln!(
            "skipping nfs_mount_serves_blob_content: \
             set HEDDLE_NFS_AVAILABLE=1 (with sudo/admin) to enable"
        );
        return;
    }

    // Build a tiny repo with one captured file.
    let repo_dir = TempDir::new().unwrap();
    let repo = Repository::init_default(repo_dir.path()).unwrap();
    std::fs::write(repo_dir.path().join("hello.txt"), b"world").unwrap();
    repo.snapshot(Some("fixture".into()), None).unwrap();

    let mount = ContentAddressedMount::new(repo, "main").unwrap();
    let mountpoint = TempDir::new().unwrap();

    let session = NfsShell::new(mount)
        .mount_background(mountpoint.path())
        .expect("nfs mount_background");

    // NFS mounts are visible synchronously after `mount(8)` returns
    // on Linux/macOS, but Windows may need a short poll for the
    // drive letter to appear in the namespace.
    let target = mountpoint.path().join("hello.txt");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !target.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let read = std::fs::read_to_string(&target).expect("read mounted file");
    assert_eq!(read, "world");
    drop(session);
}
