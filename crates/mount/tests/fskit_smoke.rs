// SPDX-License-Identifier: Apache-2.0
//! macOS FSKit end-to-end mount test.
//!
//! Marked `#[ignore]` and additionally gated on the
//! `HEDDLE_FSKIT_AVAILABLE=1` env var so CI only opts in on
//! machines that can actually mount: macOS 26.0+, signed binary
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
#[ignore = "requires macOS 26.0 + FSKit entitlement; opt-in via HEDDLE_FSKIT_AVAILABLE=1"]
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
            "FSKit not available at runtime — this test requires macOS 26.0+. \
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

    // FSKit on macOS 26.0+ has no programmatic in-process
    // `mount(at:)`. A real mount requires a code-signed
    // `.fsmodule` System Extension that this CLI doesn't ship
    // yet (release-engineering follow-up, tracked in
    // `crates/mount/README.md`). Until that lands, the Swift
    // `mount(at:)` returns ENOSYS — the assertion below pins
    // the contract: construct + mount returns the documented
    // not-implemented errno, drop cleans up.
    let shell = FSKitShell::new(mount).expect("construct FSKit session");
    match shell.mount_background(mountpoint.path()) {
        Ok(session) => {
            // Future-proof: once the System Extension lands, the
            // mount succeeds and the read assertion below pins
            // the round-trip.
            let target = mountpoint.path().join("hello.txt");
            let read = std::fs::read_to_string(&target).expect("read mounted file");
            assert_eq!(read, "world");
            drop(session);
        }
        Err(err) => {
            // Accepted intermediate state while the `.fsmodule`
            // packaging is still pending — surface the errno so a
            // reviewer can confirm it's ENOSYS, not something
            // unrelated.
            let msg = err.to_string();
            assert!(
                msg.contains("Function not implemented") || msg.contains("ENOSYS"),
                "expected ENOSYS until .fsmodule lands, got: {msg}"
            );
        }
    }
}
