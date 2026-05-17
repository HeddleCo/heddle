// SPDX-License-Identifier: Apache-2.0
//! Windows ProjFS end-to-end mount test.
//!
//! Marked `#[ignore]` and additionally gated on the
//! `HEDDLE_PROJFS_AVAILABLE=1` env var so CI only opts in on
//! machines that can actually mount: Windows 10 1809+ / Server 2019+
//! with the "Projected File System" optional feature enabled, and
//! a virtualization-root path on NTFS (the default for `%TEMP%`).
//!
//! To run locally:
//!
//! ```powershell
//! $env:HEDDLE_PROJFS_AVAILABLE="1"
//! cargo test -p mount --features projfs --test projfs_smoke -- --ignored
//! ```
//!
//! Enabling the optional feature one-time (admin PowerShell):
//!
//! ```powershell
//! Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS
//! ```
//!
//! See `crates/mount/README.md` for the full set of Windows install
//! notes.

#![cfg(all(target_os = "windows", feature = "projfs"))]

use std::{
    env,
    time::{Duration, Instant},
};

use mount::{ContentAddressedMount, ProjFsShell};
use repo::Repository;
use tempfile::TempDir;

#[test]
#[ignore = "requires Windows + ProjFS optional feature; opt-in via HEDDLE_PROJFS_AVAILABLE=1"]
fn projfs_mount_serves_blob_content() {
    if env::var("HEDDLE_PROJFS_AVAILABLE").as_deref() != Ok("1") {
        eprintln!(
            "skipping projfs_mount_serves_blob_content: \
             set HEDDLE_PROJFS_AVAILABLE=1 to enable"
        );
        return;
    }
    if !ProjFsShell::is_runtime_available() {
        panic!(
            "ProjFS not available at runtime — this test requires the \
             'Projected File System' Windows optional feature. \
             Run `Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS` \
             from an admin PowerShell."
        );
    }

    // Build a tiny repo with one captured file.
    let repo_dir = TempDir::new().unwrap();
    let repo = Repository::init_default(repo_dir.path()).unwrap();
    std::fs::write(repo_dir.path().join("hello.txt"), b"world").unwrap();
    repo.snapshot(Some("fixture".into()), None).unwrap();

    let mount = ContentAddressedMount::new(repo, "main").unwrap();
    let mountpoint = TempDir::new().unwrap();

    let session = ProjFsShell::new(mount)
        .mount_background(mountpoint.path())
        .expect("projfs mount_background");

    // ProjFS hydrates placeholders lazily on access. Poll briefly
    // for the file to materialize so the test stays robust against
    // small startup races.
    let target = mountpoint.path().join("hello.txt");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !target.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let read = std::fs::read_to_string(&target).expect("read mounted file");
    assert_eq!(read, "world");
    drop(session);
}
