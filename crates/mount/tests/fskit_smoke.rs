// SPDX-License-Identifier: Apache-2.0
//! macOS FSKit end-to-end mount test.
//!
//! Marked `#[ignore]` and additionally gated on the
//! `HEDDLE_FSKIT_AVAILABLE=1` env var so CI only opts in on
//! machines that can actually mount: macOS 15.4+, signed binary
//! with the `com.apple.developer.fskit.fsmodule` entitlement,
//! and a developer machine with FSKit module loading enabled.
//!
//! To run locally:
//!
//! ```bash
//! HEDDLE_FSKIT_AVAILABLE=1 \
//!   cargo test -p mount --features fskit --test fskit_smoke -- --ignored
//! ```
//!
//! See `crates/mount/README.md` for the full set of macOS install
//! requirements (entitlement, code-signing, FSKit registration).

#![cfg(all(target_os = "macos", feature = "fskit"))]

use std::env;

use mount::{ContentAddressedMount, FSKitShell};
use repo::Repository;
use tempfile::TempDir;

#[test]
#[ignore = "requires macOS 15.4 + FSKit entitlement; opt-in via HEDDLE_FSKIT_AVAILABLE=1"]
fn fskit_mount_serves_blob_content() {
    if env::var("HEDDLE_FSKIT_AVAILABLE").as_deref() != Ok("1") {
        eprintln!(
            "skipping fskit_mount_serves_blob_content: \
             set HEDDLE_FSKIT_AVAILABLE=1 to enable"
        );
        return;
    }
    if !FSKitShell::is_runtime_available() {
        panic!(
            "FSKit not available at runtime — this test requires macOS 15.4+. \
             Either upgrade or unset HEDDLE_FSKIT_AVAILABLE."
        );
    }

    // Build a tiny repo with one captured file.
    let repo_dir = TempDir::new().unwrap();
    let repo = Repository::init_default(repo_dir.path()).unwrap();
    std::fs::write(repo_dir.path().join("hello.txt"), b"world").unwrap();
    repo.snapshot(Some("fixture".into()), None).unwrap();

    let mount = ContentAddressedMount::new(repo, "main").unwrap();
    let mountpoint = TempDir::new().unwrap();

    // The mount call currently returns ENOSYS because the Swift
    // adapter's `FSModuleHost.register` path is stubbed (see
    // `crates/mount/src/fskit/mod.rs` module docs). When that
    // lands, this test should drop the panic and fall through to
    // the real read assertion below.
    match FSKitShell::new(mount).mount_background(mountpoint.path()) {
        Ok(session) => {
            // Once the mount is real, exercise it. For now the
            // happy-path is unreachable; this branch is here so
            // wiring the real `FSModuleHost.register` flips the
            // test from an expected-failure to a passing one with
            // no further edits.
            let target = mountpoint.path().join("hello.txt");
            let read = std::fs::read_to_string(&target).expect("read mounted file");
            assert_eq!(read, "world");
            drop(session);
        }
        Err(err) => {
            panic!(
                "fskit mount returned an error (expected ENOSYS until the Swift \
                 FSModuleHost.register seam lands): {err}"
            );
        }
    }
}