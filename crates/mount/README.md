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

| Feature  | Platform | Effect |
|----------|----------|--------|
| `fuse`   | Linux    | Compiles `fuser` and the `FuseShell` adapter. |
| `fskit`  | macOS    | Compiles the Swift adapter and the `FSKitShell`. |
| `projfs` | Windows  | Compiles the `windows` ProjFS bindings and the `ProjFsShell` adapter. |
| `nfs`    | any      | Compiles the in-process NFSv3 fallback (`NfsShell`). |

All are off by default. The default build of the workspace runs
on every OS and produces a crate with the trait + core only — no
mountable shell.

```bash
# Linux dev box
cargo build -p mount --features fuse

# macOS dev box
cargo build -p mount --features fskit

# Windows dev box
cargo build -p mount --features projfs
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

1. **macOS 26.0+** for native FSKit path-backed mounts. The Swift
   adapter now uses FSKit V2 URL resources, so the CLI falls back
   to NFS on older macOS releases with a clear notice instead of
   attempting the native FSKit path.

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

# Full mount smoke test (requires macOS 26.0 + entitlements)
HEDDLE_FSKIT_AVAILABLE=1 \
  cargo test -p mount --features fskit --test fskit_smoke -- --ignored
```

The integration test will skip itself unless `HEDDLE_FSKIT_AVAILABLE=1`
is set, so a generic CI runner won't fail on it accidentally.

## Windows runtime requirements (ProjFS)

The ProjFS shell is the daily-use path on Windows. To mount, the
host needs:

1. **Windows 10 1809+, Windows 11, or Windows Server 2019+.**
   `ProjectedFSLib.dll` was added in 1809; earlier builds will
   fail at `LoadLibraryW`. [`ProjFsShell::is_runtime_available`]
   probes for the DLL and reports failure when missing.

2. **The "Projected File System" Windows optional feature
   installed.** This is NOT enabled by default on most SKUs. One-time
   enable from an admin PowerShell:

   ```powershell
   # Windows 10 / 11 client SKUs
   Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS

   # Windows Server SKUs (the feature is named differently)
   Enable-WindowsOptionalFeature -Online -FeatureName Projected-FS
   ```

   The enable is in-place and doesn't require a reboot for new
   processes. Heddle's CLI will fall back to the NFS shell if the
   feature is missing, but the user-visible performance is much
   better with ProjFS — installing it once is the recommended
   workflow.

3. **The virtualization root on NTFS.** Paths under
   `%USERPROFILE%`, `%LOCALAPPDATA%`, or any normal NTFS drive
   work. ReFS, FAT32, and exFAT volumes don't support reparse
   points and will fail at `PrjMarkDirectoryAsPlaceholder` with
   `ERROR_NOT_SUPPORTED`.

4. **The `projfs` cargo feature**, propagated through
   `heddle-cli`'s `mount` feature.

5. **No admin rights at mount time.** Once the optional feature is
   installed (one-time admin step), routine mount/unmount is
   unprivileged — the kernel-side adapter handles the privilege
   transition.

### Cleaning up a stale ProjFS mount

The clean-shutdown path is `ProjFsSession::drop`, which calls
`PrjStopVirtualizing`. If the daemon is force-killed, the
virtualization root stays *marked* (the directory keeps its
reparse metadata) but no provider answers callbacks. Reading
already-hydrated files works (they're regular NTFS files at that
point); reading a placeholder hangs the kernel until it times
out.

To clear: re-mount the same path with a fresh `ProjFsShell`
session — the sidecar GUID file in the parent directory pins the
instance identity so the kernel re-attaches without an error.
Alternatively, delete the entire virtualization root tree from
PowerShell:

```powershell
Remove-Item -Recurse -Force <root>
```

Deleting the root strips both the reparse metadata and any
hydrated placeholders. The sidecar GUID file has two possible
locations depending on parent-directory writability at mount
time — `<parent>\.<basename>.heddle-projfs-id` is the default,
`<root>\.heddle-projfs-id` is the fallback when the parent ACL
refuses writes — both should be removed to fully reset the
instance identity.

### Running the ProjFS tests locally

The unit tests in `src/projfs.rs::tests` and the smoke tests in
`tests/projfs_smoke.rs` are all `#[ignore]` by default. To run:

```powershell
# Smoke tests (require the optional feature)
$env:HEDDLE_PROJFS_AVAILABLE = "1"
cargo test -p mount --features projfs --test projfs_smoke -- --ignored

# Unit lifecycle tests (no real mount; just constructor + drop +
# is_runtime_available probe)
cargo test -p mount --features projfs --lib projfs:: -- --ignored
```

The smoke tests will skip themselves unless
`HEDDLE_PROJFS_AVAILABLE=1` is set, so a Windows host without the
optional feature won't fail them accidentally.

## Per-thread overlay semantics (write path)

Every mount is bound to exactly one thread (`mount.thread`). All
writes the kernel issues against the mountpoint land in a
**per-thread overlay** layered on top of the underlying CAS; the
captured tree is never mutated in place. The overlay is the diff
between "what the thread looked like at mount time" and "what the
agent has written since".

The overlay has six pieces, all in process memory until
[`ContentAddressedMount::capture`] folds them into a new heddle
state:

| Piece                  | What it holds                                       | Promoted on `capture` as                              |
|------------------------|-----------------------------------------------------|-------------------------------------------------------|
| `hot` buffer           | In-flight `pwrite` bytes per open NodeId            | Drained to `warm` first, then folded as a file blob   |
| `warm` tier            | Path → CAS-promoted blob (post-`flush`/`close`)     | File blob in the destination tree                     |
| `tombstones`           | Paths the mount has `unlink`'d                      | Removes the captured entry; prunes empty parent dirs  |
| `dir_tombstones`       | Captured-tree directories the mount has `rmdir`'d   | Drops the whole subtree from the destination tree     |
| `explicit_dirs`        | Empty `mkdir`s with no children yet                 | Materialized as 0-entry subtree                       |
| `symlinks`             | Path → link target bytes                            | Hashed once, planted as a `Symlink` tree entry        |

Lock ordering inside the mount is **state → pending → inodes**;
every write-side op holds them in that order to avoid the deadlock
shapes Codex caught early in development. See the `MountInner`
docstring in `src/core.rs`.

### What `capture` does

When the agent runs `heddle capture`, the orchestrator calls
[`ContentAddressedMount::capture`] which:

1. Drains every open hot buffer to the warm tier (no agent ever
   "loses" an in-flight write — the close handshake is the only
   point of fragility, and the safety-sweep thread covers the
   process-killed-without-close case via the `idle_after` window).
2. Folds the warm tier + tombstones + dir_tombstones +
   explicit_dirs + symlinks into a fresh root tree, descending into
   each pending sub-path and merging against the captured
   counterpart. Empty captured-dir entries prune naturally.
3. Records a new `State` referencing that root tree and advances
   the thread's HEAD. Attribution comes from the repo's default
   path (`HEDDLE_AGENT_*` env + repo config + principal).
4. Clears the entire pending tier. The next write starts with a
   fresh overlay.

The whole pass is one walk of the pending map — no worktree-scan,
no re-hash of any blob the agent already wrote through `flush`.
The result is content-addressed: two agents that wrote identical
bytes to different paths produce **one** CAS blob, not two.

### What does NOT persist across capture

The overlay is intentionally lightweight; some kernel-side concepts
don't have a tree-level home and surface as no-ops or
approximations:

- **`chmod` other than `+x`.** Heddle's [`FileMode`] is a three-way
  enum (`Normal` / `Executable` / `Symlink`); arbitrary 9-bit perm
  masks fold into the closest mode at capture time. The
  user-executable bit (`0o100`) flips Normal ↔ Executable; the
  rest of the bits (group/other read/write, setuid, sticky) are
  not modelled and don't survive `capture`.
- **`chown` / uid / gid.** The mount reports every node as owned by
  the mount-owner's uid + primary gid. `setattr(uid)` /
  `setattr(gid)` are accepted as no-ops so callers don't get
  `EPERM` from a chown that wouldn't have had a visible effect
  anyway.
- **Per-node mtime / atime / ctime.** Every node reports the
  mount's `mounted_at` timestamp. `setattr(mtime)` / `utimensat`
  are accepted as no-ops.
- **Hard links.** `link(2)` returns `EPERM`. The CAS already
  de-duplicates by content hash, so two paths with the same bytes
  share one blob — but they're independent tree entries, not
  aliased inodes.
- **`mknod` for device / FIFO / socket nodes.** Only regular files
  (`S_IFREG`) are accepted; everything else returns `EPERM`.
  Heddle's tree doesn't model device files.
- **Captured-directory rename.** Renaming an entirely overlay-only
  directory (one that the agent `mkdir`'d in this session) works;
  renaming a captured directory returns `EINVAL`. Cross-tree
  directory renames would need a recursive tombstone + warm-tier
  rewrite pass that's out of scope for the cargo / git / npm
  daily-use path.

### What about `inotify` / `fanotify`?

Not delivered. The mount doesn't emit filesystem-change events
for editors / file-watchers / `cargo watch`-style tooling. This
is a separate substrate decision tracked in a follow-up issue;
the cargo / git path doesn't need it.

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
- Windows ProjFS shell: production-ready (heddle#75). The CLI
  routes Windows daily-use traffic through `ProjFsShell` with an
  NFS fallback when the optional feature is missing. Production
  hardening (panic recovery in every trampoline, dir-enumeration
  cursor across multi-call `get_dir_enum`, instance-ID sidecar
  outside the projection envelope, close-modified bridge for the
  write path) is locked in by the `projfs-smoke` CI matrix entry;
  see `.github/workflows/rust-tests.yml` and
  `tests/projfs_smoke.rs`. CfAPI (the cloud-files variant) is
  still unimplemented and a follow-up only if a remote-fetch
  product surface lands.

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
