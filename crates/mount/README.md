# mount

Heddle's content-addressed mount. The crate exposes virtualized
threads as a directory tree backed by the object store.

## Architecture

```
PlatformShell trait         ← thin platform adapters
  (FuseShell, FSKitShell)     (one per OS)
        ↓
ContentAddressedMount       ← pure Rust core
        ↓
crates/repo + crates/objects
```

The trait is in `src/shell.rs`. The core is in `src/core.rs`. Each
adapter is its own `cfg`-gated module.

## Cargo features

| Feature | Platform | Effect |
|---------|----------|--------|
| `fuse`  | Linux    | Compiles `fuser` and the `FuseShell` adapter. |
| `fskit` | macOS    | Compiles the Swift adapter and the `FSKitShell`. |

Both are off by default. The default build of the workspace runs
on every OS and produces a crate with the trait + core only — no
mountable shell.

```bash
# Linux dev box
cargo build -p mount --features fuse

# macOS dev box
cargo build -p mount --features fskit
```

## macOS install requirements (FSKit)

The FSKit shell needs:

1. **macOS 15.4+** at runtime. The Swift adapter compiles against
   the 14.0 SDK and weak-links FSKit, so the build works on older
   SDKs too — but `mount_background` will fail at runtime on
   anything below 15.4. Use `FSKitShell::is_runtime_available()`
   to probe.

2. **Xcode command line tools** (`xcode-select --install`). The
   `build.rs` invokes `swiftc` to compile
   `swift/HeddleFSKit/HeddleFSKit.swift` into a static library.
   Override the path via `HEDDLE_SWIFTC=/path/to/swiftc` and the
   Swift runtime location via `HEDDLE_SWIFT_RUNTIME=/path/to/usr/lib/swift/macosx`.

3. **The `com.apple.developer.fskit.fsmodule` entitlement** on the
   binary that loads the FSKit module. Apple gates this entitlement
   to apps with a paid Developer account; the binary must be
   code-signed with a provisioning profile that grants it. For
   distribution, that means a Developer ID certificate + the
   FSKit entitlement attached to the heddle CLI binary.

4. **A code-signed build.** `codesign --entitlements heddle.entitlements
   --sign "Developer ID Application: ..." target/release/heddle`.
   For local development you can self-sign with an ad-hoc identity,
   but the kernel will refuse to load an FSKit module from an
   unsigned binary on production macOS configurations.

5. **(Optional) Disable SIP** for local development if you want to
   skip the entitlement requirement. Not recommended; only useful
   for proof-of-concept work on a dev VM.

### Running the FSKit tests locally

The unit tests in `src/fskit/mod.rs` and the smoke test in
`tests/fskit_smoke.rs` are all `#[ignore]` by default. To run:

```bash
# Unit lifecycle test (no real mount; just constructor + drop)
cargo test -p mount --features fskit fskit::tests -- --ignored

# Full mount smoke test (requires macOS 15.4 + entitlements)
HEDDLE_FSKIT_AVAILABLE=1 \
  cargo test -p mount --features fskit --test fskit_smoke -- --ignored
```

The integration test will skip itself unless `HEDDLE_FSKIT_AVAILABLE=1`
is set, so a generic CI runner won't fail on it accidentally.

## Status

- Linux FUSE shell: production. Used by the heddle CLI when
  `--features mount` is enabled.
- macOS FSKit shell: scaffolded. The C ABI between the Swift
  adapter and the Rust trampolines is wired end-to-end; the
  `PlatformShell` dispatch is exercised by the unit test. The
  `FSModuleHost.register(...)` step that publishes the volume to
  the kernel is **stubbed** — the Swift `mount(at:)` returns
  `ENOSYS` today. Finishing this needs a `.fsmodule` bundle and
  the FSKit entitlement, which are release-engineering tasks.
- Windows ProjFS / CfAPI shell: not started.

## CLI integration

The CLI's mount lifecycle (`crates/cli/src/cli/commands/mount_lifecycle.rs`)
currently dispatches through `FuseShell` only — it's gated on
`#[cfg(all(target_os = "linux", feature = "mount"))]`. Adding
FSKit dispatch is a follow-up:

1. Add a `#[cfg(all(target_os = "macos", feature = "mount"))]`
   sibling module that mirrors the existing `linux` module but
   uses `FSKitShell` and `FSKitSession`.
2. Re-export `MountHandle` / `spawn_mount_for_thread` /
   `unmount_thread_if_mounted` from whichever module is
   compiled in.
3. Add the `fskit` feature to the CLI's `mount` feature
   propagation in `crates/cli/Cargo.toml`.

The trait layer means the dispatch site stays one line per
backend; nothing in `mount_lifecycle.rs`'s consumer code needs to
change.
