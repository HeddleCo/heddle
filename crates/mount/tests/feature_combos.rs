// SPDX-License-Identifier: Apache-2.0
//! Feature-combo compile markers.
//!
//! These tests exist purely so a `cargo test -p mount` invocation
//! produces one visible pass per `(target, feature)` combo we
//! expect to compile. The actual matrix is driven by CI (see
//! `.github/workflows/*.yml`); each job runs one of:
//!
//! ```bash
//! cargo check -p mount --no-default-features                # every target
//! cargo check -p mount --features fuse                      # Linux only
//! cargo check -p mount --features fskit                     # macOS only
//! cargo check -p mount --features projfs                    # Windows only
//! cargo check -p mount --all-features                       # every target
//! ```
//!
//! The body of each marker is trivial — the point is that the
//! `#[cfg]` arm compiles. A regression that breaks (say) the
//! `--all-features` build on Linux will fail this test file before
//! any platform-specific test even runs.

#[test]
fn mount_crate_compiles_no_features() {
    // Default feature set. The crate is platform-agnostic with
    // no backend selected. Must work on every target.
    let _ = mount::NodeId::ROOT;
}

#[cfg(all(target_os = "linux", feature = "fuse"))]
#[test]
fn mount_crate_compiles_with_fuse_on_linux() {
    // FUSE adapter must be in scope when the feature is on.
    let _ = std::any::type_name::<mount::FuseShell>();
}

#[cfg(all(target_os = "macos", feature = "fskit"))]
#[test]
fn mount_crate_compiles_with_fskit_on_macos() {
    let _ = std::any::type_name::<mount::FSKitShell>();
}

#[cfg(all(target_os = "windows", feature = "projfs"))]
#[test]
fn mount_crate_compiles_with_projfs_on_windows() {
    let _ = std::any::type_name::<mount::ProjFsShell>();
}

#[cfg(feature = "nfs")]
#[test]
fn mount_crate_compiles_with_nfs() {
    // NFS is the universal fallback — not target-gated. Must
    // compile on every target where the workspace builds.
    let _ = std::any::type_name::<mount::NfsShell>();
}
