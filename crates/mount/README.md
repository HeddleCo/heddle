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

## Linux runtime requirements (FUSE)

The FUSE shell is the daily-use path on Linux. To mount, the host
needs:

1. **`/dev/fuse` available and readable by the mount-owner.** Most
   distros ship this enabled in the kernel; containers may need
   `--device=/dev/fuse --cap-add SYS_ADMIN`.

2. **`fusermount3` on `PATH`.** This is the SUID helper that
   actually publishes the mount to the kernel. Debian/Ubuntu ships
   it in `fuse3`; RHEL/Fedora in `fuse-libs` + `fuse3`. Install with:

   ```bash
   sudo apt-get install fuse3 libfuse3-dev     # Debian/Ubuntu
   sudo dnf install fuse3 fuse3-devel          # Fedora/RHEL
   ```

3. **No special `/etc/fuse.conf` configuration is required for the
   default mount.** The shell uses `Owner`-scoped ACL (the kernel
   gates access to the mountpoint at the user level), so
   `user_allow_other` is not needed. If you want to allow other
   local users (or root) to read into the mount — uncommon for
   heddle's single-user workflow — that's where `user_allow_other`
   in `/etc/fuse.conf` comes in, paired with explicit
   `MountOption::AllowOther` / `AllowRoot` in
   [`crate::fuse::default_config`]. The default shell does *not*
   enable `AutoUnmount` for this reason: fuser 0.17 rejects
   `AutoUnmount` without `AllowOther`/`AllowRoot`, and we'd rather
   require an explicit unmount path than depend on host-level
   policy.

4. **The `fuse` cargo feature**, propagated through
   `heddle-cli`'s `mount` feature.

5. **Linux 5.16+ for shared `mmap` on mounted files.** The shell
   hands back `FOPEN_DIRECT_IO` on every `open` so kernel page-cache
   reads don't shadow hot-tier writes (see
   [`crate::fuse::FuseShell::open`]). Under default kernel semantics
   that disables `mmap(MAP_SHARED, ...)` on every fd — calls return
   `ENODEV`. The shell opts out of that restriction by requesting
   the `FUSE_DIRECT_IO_ALLOW_MMAP` capability in `init`; the kernel
   bit was added in 5.16. On older kernels the cap is silently
   dropped and shared mmap fails with `ENODEV` — rust-analyzer,
   cargo (when reading dep-info files via `mmap`), IDEs, and
   `grep --mmap` will misbehave on heddle-mounted trees. The mount
   itself still works; only the mmap path is affected. Upgrade the
   kernel or use clients that fall back to `read(2)`.

### Cleaning up a stale FUSE mount

The clean-shutdown path is a `BackgroundSession::drop` (the CLI's
`MountHandle::unmount`). If the daemon is `kill -9`'d or the
session is leaked, the mount lingers — userspace sees the
content-addressed view, but no daemon is answering. To clear:

```bash
fusermount3 -u <mountpoint>
```

If `fusermount3 -u` fails with `Device or resource busy`, kill any
process with a working directory or open file descriptor under the
mountpoint (`lsof <mountpoint>`), then retry. As a last resort,
`umount -l <mountpoint>` does a lazy detach — the kernel hides the
mount from new accesses while waiting for outstanding handles to
close.

### Errno mapping

The shell translates a `MountError` into a kernel-replied errno via
[`crate::error::MountError::to_errno`]. Today's mapping:

| `MountError` variant | errno     | Surfaced as |
|----------------------|-----------|-------------|
| `NotFound`           | `ENOENT`  | "No such file or directory" |
| `UnknownThread`      | `ENOENT`  | (a thread name we can't resolve looks like a path miss to userspace) |
| `Stale`              | `ESTALE`  | "Stale file handle" — usually the inode was invalidated mid-flight |
| `NotADirectory`      | `ENOTDIR` | "Not a directory" |
| `ReadOnly`           | `EROFS`   | "Read-only file system" |
| `Store(NotFound)`    | `ENOENT`  | underlying object missing |
| `Store(Io)`          | passthrough | the wrapped `std::io::Error`'s `raw_os_error()` if present, else `EIO` |
| `Store(_)`           | `EIO`     | "Input/output error" — generic catch-all for unmapped object-store errors |

Any panic deep in a `PlatformShell` call surfaces to userspace as
`EIO` (the FUSE shell wraps each callback in `guard_call`; see
[`crate::fuse`] module docs). Without that guard, a single panic in
a worker thread would either wedge the mount (kernel waits for
replies that never come) or — post Rust 1.81 — abort the daemon
process across an `extern "C"` boundary inside `fuser`. The
`fuse::tests::guard_call_translates_panic_to_eio` test pins the
contract.

### Platform feature parity

The Linux FUSE shell, macOS FSKit shell, and Windows ProjFS shell
all share the same [`PlatformShell`] core, so the set of
**heddle-visible** operations is identical across platforms:
`lookup`, `read`, `write`, `enumerate`, `attrs`, `flush`, `release`,
`invalidate`. What differs is the **kernel-side capabilities** each
backend exposes to userspace:

| Capability             | FUSE (Linux)         | FSKit (macOS)        | ProjFS (Windows)     |
|------------------------|----------------------|----------------------|----------------------|
| Per-write callbacks    | yes                  | yes                  | no (close-time only) |
| Extended attributes    | not wired            | not wired            | n/a                  |
| `mkdir` / `create`     | not wired (ENOSYS)   | not wired            | not wired            |
| `setattr(size)` (O_TRUNC) | not wired (ENOSYS)   | not wired            | n/a                  |
| Symlink readback       | works (read-only)    | works (read-only)    | works (read-only)    |
| Shared `mmap` on mounted files | yes (kernel 5.16+; falls back to ENODEV on older kernels) | yes (UBC-backed) | yes (projected files are physical on the volume) |
| ACLs / chmod through mount | not honored      | not honored          | n/a                  |
| Auto-unmount on daemon death | off by default | n/a                  | n/a                  |
| Panic-recovery in callbacks | yes (`guard_call`) | yes (`guarded_c_int`) | yes (`guarded_hresult`) |

"Not wired" means heddle's adapter doesn't override the framework's
default — typically `ENOSYS`. Wiring an op is a matter of
implementing the relevant trait method *and* adding a
[`PlatformShell`] hook for it; the existing seven ops are sufficient
for the daily "mount → read → edit-existing-file → capture" loop
that drives the 2-day milestone.

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
  `--features mount` is enabled. Production hardening (panic
  recovery in every callback, write-through-mount round-trip,
  concurrent-read smoke, clean unmount on drop) is locked in by
  the `fuse-smoke` CI matrix entry; see
  `.github/workflows/rust-tests.yml` and `tests/fuse_mount.rs`.
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
