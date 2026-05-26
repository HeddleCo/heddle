// SPDX-License-Identifier: Apache-2.0
//! Build script for the `mount` crate.
//!
//! Only does work when `feature = "fskit"` is enabled on macOS.
//!
//! Two steps, in order:
//!
//!   1. Regenerate `swift/HeddleFSKit/HeddleFSKit-Bridging.h` from
//!      `src/fskit/c_abi.rs` via `cbindgen`. The Rust module is the
//!      single source of truth for the C ABI; the Swift bridging
//!      header is downstream of it. This means: add a callback to
//!      `c_abi.rs`, build, and the Swift compiler either sees the
//!      new typedef and links cleanly, or — if the Swift call sites
//!      have drifted — fails loudly at `swiftc` rather than at
//!      runtime.
//!
//!   2. Compile the Swift adapter at
//!      `swift/HeddleFSKit/HeddleFSKit.swift` (which #imports the
//!      generated header) into a static library and link it.

use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    // Always re-run if the build script itself or the cbindgen
    // config changes.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=cbindgen.toml");

    // Off unless the feature is on AND we're targeting macOS. The
    // feature check needs to be a CARGO_FEATURE_* env probe because
    // build.rs runs before features are reflected in `cfg`.
    let fskit_enabled = env::var_os("CARGO_FEATURE_FSKIT").is_some();
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if !fskit_enabled {
        return;
    }
    if target_os != "macos" {
        // Feature can be enabled on a non-macOS host (e.g. via
        // `--all-features` on Linux). The Rust module is `cfg`-gated
        // to macOS, so just no-op here and let the feature be inert.
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let swift_dir = manifest_dir.join("swift").join("HeddleFSKit");
    let swift_src = swift_dir.join("HeddleFSKit.swift");
    let bridging = swift_dir.join("HeddleFSKit-Bridging.h");
    let c_abi_src = manifest_dir.join("src").join("fskit").join("c_abi.rs");

    println!("cargo:rerun-if-changed={}", swift_src.display());
    println!("cargo:rerun-if-changed={}", c_abi_src.display());

    // 1. Regenerate the Swift bridging header from the canonical
    //    Rust C-ABI module. cbindgen writes the file in-place; if
    //    the contents differ from the previous build, the Swift
    //    compile in step 2 picks up the new shape.
    regenerate_bridging_header(&manifest_dir, &bridging);
    println!("cargo:rerun-if-changed={}", bridging.display());

    // 2. Compile the Swift source to a single object file.
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let static_lib = out_dir.join("libHeddleFSKit.a");
    let object = out_dir.join("HeddleFSKit.o");

    // `-parse-as-library`: this is a library, not a script with a
    //   top-level entry point.
    // `-emit-object`: produce a .o, not a .swiftmodule.
    // `-static`: emit code for a static lib (no `_swift_FORCE_LOAD_*`
    //   weak refs that need a Swift dylib at runtime).
    // `-target arm64-apple-macos14.0`: FSKit needs >= 14.0 SDK to
    //   compile (the actual runtime needs 15.4 — handled at runtime
    //   via `heddle_fskit_is_available`).
    // `-import-objc-header`: import the cbindgen-generated bridging
    //   header so Swift's call sites and the C ABI declared in
    //   `c_abi.rs` are co-resolved by the Swift compiler. Note that
    //   Swift 6.x's `@_cdecl` does not currently validate parameter
    //   shapes against the imported header, so the load-bearing
    //   drift defense remains the `fskit_bridging_header_declares_*`
    //   integration test plus the build-script regeneration. The
    //   import is kept for surface visibility (Swift sees the C
    //   typedefs alongside its own).
    let target = swift_target_triple();
    let swiftc = env::var("HEDDLE_SWIFTC").unwrap_or_else(|_| "swiftc".into());
    let status = Command::new(&swiftc)
        .args([
            "-parse-as-library",
            "-emit-object",
            "-static",
            "-O",
            "-target",
            &target,
            "-module-name",
            "HeddleFSKit",
            "-import-objc-header",
        ])
        .arg(&bridging)
        .args(["-o"])
        .arg(&object)
        .arg(&swift_src)
        .status()
        .expect("failed to invoke swiftc; install Xcode command line tools");
    assert!(
        status.success(),
        "swiftc failed for HeddleFSKit.swift (status: {status})"
    );

    // 3. Bundle the object into a static archive.
    let status = Command::new("ar")
        .arg("crus")
        .arg(&static_lib)
        .arg(&object)
        .status()
        .expect("failed to invoke ar");
    assert!(status.success(), "ar failed assembling libHeddleFSKit.a");

    // 4. Tell rustc to link the archive plus the Swift runtime.
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=HeddleFSKit");

    // Swift runtime support. `-rpath` so the resulting binary can
    // find `libswiftCore.dylib` at run time without DYLD_LIBRARY_PATH.
    let toolchain = swift_runtime_path();
    println!("cargo:rustc-link-search=native={toolchain}");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{toolchain}");

    // FSKit weak-links so a build on 15.0 still loads (the runtime
    // gate above blocks actual mounts on < 15.4).
    println!("cargo:rustc-link-arg=-weak_framework");
    println!("cargo:rustc-link-arg=FSKit");
    println!("cargo:rustc-link-arg=-framework");
    println!("cargo:rustc-link-arg=Foundation");
}

/// Run cbindgen against the canonical `c_abi.rs` source file and
/// write the result to the Swift bridging header path. Loads
/// `cbindgen.toml` from the crate root for stylistic config.
///
/// Note: we use `with_src(c_abi.rs)` instead of `with_crate(..)` so
/// cbindgen's parse phase only sees the FSKit-relevant declarations.
/// Pointing it at the whole crate would pull `pub` newtypes (e.g.
/// `NodeId`) into the generated header as forward declarations,
/// which would silently leak Rust-internal types into the Swift
/// bridging surface.
#[cfg(feature = "fskit")]
fn regenerate_bridging_header(manifest_dir: &Path, output: &Path) {
    let config = cbindgen::Config::from_file(manifest_dir.join("cbindgen.toml"))
        .expect("failed to load cbindgen.toml");

    let c_abi = manifest_dir.join("src").join("fskit").join("c_abi.rs");
    let bindings = cbindgen::Builder::new()
        .with_src(&c_abi)
        .with_config(config)
        .generate()
        .expect("cbindgen failed to generate FSKit bridging header");

    // `write_to_file` returns true if the file changed. We don't
    // care about the answer — we just want the header on disk.
    bindings.write_to_file(output);
}

/// Stub used when the `fskit` feature is off so the build script
/// type-checks on every host. The fast-return at the top of `main`
/// means this is never actually reached.
#[cfg(not(feature = "fskit"))]
fn regenerate_bridging_header(_: &Path, _: &Path) {
    unreachable!("regenerate_bridging_header invoked without fskit feature");
}

fn swift_target_triple() -> String {
    // Mirror cargo's target arch when possible, defaulting to the
    // host arch. Apple-silicon dev box, Intel CI — both work.
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "aarch64".into());
    let normalized = match arch.as_str() {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        other => other,
    };
    format!("{normalized}-apple-macos14.0")
}

fn swift_runtime_path() -> String {
    // Default to the Xcode toolchain location. Override via
    // HEDDLE_SWIFT_RUNTIME for non-standard installs.
    env::var("HEDDLE_SWIFT_RUNTIME").unwrap_or_else(|_| {
        "/Applications/Xcode.app/Contents/Developer/Toolchains/\
         XcodeDefault.xctoolchain/usr/lib/swift/macosx"
            .into()
    })
}
