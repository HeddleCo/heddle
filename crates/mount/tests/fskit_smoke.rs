// SPDX-License-Identifier: Apache-2.0
//! macOS FSKit session bootstrap smoke test.
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
fn fskit_session_constructs_for_extension_bootstrap() {
    if env::var("HEDDLE_FSKIT_AVAILABLE").as_deref() != Ok("1") {
        eprintln!(
            "skipping fskit_session_constructs_for_extension_bootstrap: \
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
    let shell = FSKitShell::new(mount).expect("construct FSKit session");
    drop(shell);
}
