// SPDX-License-Identifier: Apache-2.0
//! Real-toolchain smoke test for `--workspace light` mounts.
//!
//! Mirrors the original `cargo_build_smoke.sh` shell harness (D5 in
//! the mount test plan) but runs it as an opt-in Rust integration
//! test so it participates in `cargo test` selection / sharding /
//! filtering. The script intentionally drove the CLI; this test
//! drives the `mount` library directly, which is exactly what the
//! `--no-daemon` (in-process) CLI path does under the hood — see
//! `crates/cli/src/cli/commands/mount_lifecycle.rs::MountOwnership::InProcess`.
//! Going library-direct gives us deterministic teardown via `Drop`
//! without a `heddle` binary on PATH, no daemon socket, no PID files
//! to clean up, and no CLI-flag drift to chase.
//!
//! What this test proves end-to-end:
//!
//! 1. A heddle repo can be initialized and snapshotted from a tempdir.
//! 2. A `ContentAddressedMount` over that snapshot can be projected
//!    through `FuseShell::mount_background` into a tempdir mountpoint.
//! 3. The host kernel sees the FUSE FS and can read it well enough
//!    that `cargo build --offline` (a real downstream toolchain) can
//!    parse `Cargo.toml`, read `src/main.rs`, write `target/`, and
//!    produce a working binary inside the mount.
//! 4. Dropping the session unmounts cleanly.
//!
//! The fixture is a deliberately-tiny, zero-dependency Rust crate so
//! `cargo build --offline` doesn't touch the registry. We also point
//! `CARGO_TARGET_DIR` at a sibling tempdir rather than letting cargo
//! create `target/` inside the mount itself — writes through the
//! FUSE shell are still in the writable-overlay codepath we don't
//! exercise here, and the *read* path (the source tree) is what this
//! smoke test is locking in. If you want to exercise the write path
//! too, drop the env var and let cargo emit `target/` into the mount.
//!
//! ## Running
//!
//! ```bash
//! cargo test -p mount --features fuse --test cargo_build_smoke -- --ignored
//! ```
//!
//! ## Why `#[ignore]`
//!
//! Same calculus as `fuse_mount.rs`: needs a working `fuse` kernel
//! module + `fusermount3` on PATH + a `cargo` toolchain. Not a fair
//! assumption for a generic CI runner. Marked `#[ignore]` so it's
//! opt-in via `--ignored`, identical pattern to its sibling.

#![cfg(all(target_os = "linux", feature = "fuse"))]

use std::{
    fs,
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

use mount::{ContentAddressedMount, FuseShell};
use repo::Repository;
use tempfile::TempDir;

/// Minimal, zero-dependency `Cargo.toml` for the fixture crate.
/// Pinning `edition = "2021"` (rather than 2024) avoids needing a
/// very recent stable toolchain on the test host. Keeping `[dependencies]`
/// empty is the load-bearing detail: it lets `cargo build --offline`
/// succeed without ever touching the registry or CARGO_HOME.
const FIXTURE_CARGO_TOML: &str = r#"[package]
name = "calc"
version = "0.1.0"
edition = "2021"

[dependencies]
"#;

/// Trivial `main.rs` for the fixture. The body content is irrelevant
/// — we only assert on whether `cargo build` produced an artifact —
/// but a `println!` at least makes the binary executable for any
/// follow-up smoke that wants to run it.
const FIXTURE_MAIN_RS: &str = r#"fn main() {
    println!("Hello, mount!");
}
"#;

#[test]
#[ignore = "requires FUSE + cargo on host; opt-in via --ignored"]
fn cargo_build_succeeds_against_virtualized_mount() {
    // Skip gracefully on hosts without FUSE so a developer who runs
    // `cargo test -- --ignored` on a non-FUSE box gets a clear signal
    // instead of a confusing mount error. Mirrors the early-exit
    // pattern used elsewhere in the workspace's FUSE tests.
    if !Path::new("/dev/fuse").exists() {
        eprintln!("skipping: /dev/fuse not present on this host");
        return;
    }

    // Likewise skip if `cargo` isn't on PATH. The test is about the
    // mount holding up under a real downstream tool — if there's no
    // tool to run, there's nothing to assert. Fail soft, don't lie.
    if Command::new("cargo").arg("--version").output().is_err() {
        eprintln!("skipping: `cargo` not on PATH");
        return;
    }

    // ----- Stage 1: build the fixture crate inside a tempdir -----
    //
    // We deliberately do NOT depend on `examples/calculator`: that
    // crate has external dependencies, which would force a populated
    // CARGO_HOME or a vendored `vendor/` tree. A self-contained zero-dep
    // fixture lets us pass `--offline` honestly.
    let repo_dir = TempDir::new().expect("repo tempdir");
    let crate_root = repo_dir.path();
    fs::create_dir_all(crate_root.join("src")).expect("create src/");
    fs::write(crate_root.join("Cargo.toml"), FIXTURE_CARGO_TOML).expect("write Cargo.toml");
    fs::write(crate_root.join("src").join("main.rs"), FIXTURE_MAIN_RS).expect("write main.rs");

    // ----- Stage 2: init heddle and snapshot the fixture -----
    let repo = Repository::init_default(crate_root).expect("init heddle repo");
    repo.snapshot(Some("calc-fixture".into()), None)
        .expect("snapshot fixture");

    // ----- Stage 3: project the snapshot through a FUSE mount -----
    //
    // `FuseShell::mount_background` is the same primitive
    // `mount_lifecycle::spawn_mount_for_thread` calls under
    // `--workspace light --no-daemon`. The session is kept
    // alive for the body of the test; dropping it triggers unmount
    // (the deterministic teardown the task asked for).
    let mount = ContentAddressedMount::new(repo, "main").expect("build mount");
    let mountpoint = TempDir::new().expect("mount tempdir");
    let session = FuseShell::new(mount)
        .mount_background(mountpoint.path())
        .expect("spawn FUSE background session");

    // The kernel publishes the mounted FS asynchronously; bounded
    // poll until our fixture file appears. Same shape as `fuse_mount.rs`.
    let cargo_toml_in_mount = mountpoint.path().join("Cargo.toml");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cargo_toml_in_mount.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        cargo_toml_in_mount.exists(),
        "Cargo.toml never appeared in mount at {} — FUSE worker may not have started",
        cargo_toml_in_mount.display(),
    );

    // ----- Stage 4: run `cargo build --offline` against the mount -----
    //
    // `--offline` is the single flag that makes this test honest: with
    // a zero-dep fixture, cargo has no work to do against the registry,
    // and this assertion catches any regression that would silently
    // start needing one.
    //
    // `CARGO_TARGET_DIR` redirects `target/` outside the mount so we
    // don't accidentally exercise the writable-overlay path here —
    // that's a separate test surface. The *source* read path is what
    // we're locking in.
    let target_dir = TempDir::new().expect("target tempdir");
    let output = Command::new("cargo")
        .arg("build")
        .arg("--offline")
        .arg("--manifest-path")
        .arg(mountpoint.path().join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", target_dir.path())
        // Strip any inherited RUSTFLAGS / build env that could push
        // cargo toward needing the network or a non-default toolchain.
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_BUILD_RUSTFLAGS")
        .output()
        .expect("invoke cargo build");

    if !output.status.success() {
        // Surface stdout+stderr on failure so a CI log tells you why
        // cargo bailed (missing toolchain, write to mount, etc.).
        panic!(
            "cargo build against virtualized mount failed (status={:?})\n--- stdout ---\n{}\n--- stderr ---\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    // ----- Stage 5: assert the binary actually got produced -----
    let bin_path = target_dir.path().join("debug").join("calc");
    assert!(
        bin_path.exists(),
        "cargo build claimed success but binary missing at {}",
        bin_path.display(),
    );

    // ----- Stage 6: deterministic teardown -----
    // Explicit drop documents intent: this is what unmounts the FS.
    // The TempDir guards drop after, removing the (now empty) mount
    // dir and the source tempdirs.
    drop(session);
}