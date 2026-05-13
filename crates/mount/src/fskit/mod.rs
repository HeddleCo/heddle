// SPDX-License-Identifier: Apache-2.0
//! macOS FSKit shell.
//!
//! [`FSKitShell`] mirrors [`crate::fuse::FuseShell`] for macOS 15.4+.
//! The kernel-side adapter is FSKit (Apple's blessed userspace-FS
//! framework), reached through a small Swift shim at
//! `crates/mount/swift/HeddleFSKit/`. The shim exposes a hand-rolled
//! C ABI that this module dispatches to a [`PlatformShell`].
//!
//! ## Layout
//!
//! ```text
//!   FSKit framework  (kernel-side)
//!         ▲
//!         │  Swift protocols (FSUnaryFileSystem, FSVolume, FSItem)
//!         │
//!   HeddleFSKit.swift  ← the only Swift code in the workspace
//!         ▲
//!         │  C ABI (this module's `extern "C"` declarations)
//!         │
//!   FSKitShell  ← this file
//!         ▲
//!         │  PlatformShell trait
//!         │
//!   ContentAddressedMount  ← pure Rust core, shared with FuseShell
//! ```
//!
//! ## Realistic scope
//!
//! The C ABI, the Rust trampolines, the Swift session lifecycle,
//! and the [`PlatformShell`] dispatch are all wired. The piece
//! that's stubbed is the actual `FSModuleHost.register(...)` call
//! that publishes the volume to the kernel — that needs a
//! code-signed `.fsmodule` bundle and the `fskit.fsmodule`
//! entitlement, which is a release-engineering task, not a coding
//! task. See `crates/mount/README.md` for the install steps.
//!
//! What works today:
//!   * `FSKitShell::new` constructs a session; `Drop` releases it.
//!   * Every kernel callback FSKit will eventually invoke is
//!     wired through to the trait.
//!   * [`Self::is_runtime_available`] reports whether the host can
//!     actually load FSKit.
//!
//! What's stubbed (returns `ENOSYS` on `mount_background`):
//!   * `FSModuleHost` registration.
//!   * Programmatic `FSResource.mount(...)` invocation.

use std::{
    ffi::{CStr, CString, OsStr, c_char, c_int, c_void},
    os::unix::ffi::OsStrExt,
    path::Path,
    sync::Arc,
    time::SystemTime,
};

use tracing::warn;

use crate::{
    core::ContentAddressedMount,
    error::{MountError, Result},
    shell::{Attrs, DIR_UNIX_MODE, Entry, NodeId, NodeKind, PlatformShell},
};

pub mod c_abi;

// The C ABI is declared *once*, in [`c_abi`]. The Swift bridging
// header (`swift/HeddleFSKit/HeddleFSKit-Bridging.h`) is regenerated
// from that module by `cbindgen` at build time. Use these aliases
// throughout this file so any signature change in `c_abi` propagates
// here at the type-checker, not at runtime.
use c_abi::{
    HeddleEnumerateEmit, HeddleFSKitSessionHandle, heddle_fskit_is_available,
    heddle_fskit_session_free, heddle_fskit_session_mount, heddle_fskit_session_new,
    heddle_fskit_session_unmount,
};

// ----------------------------------------------------------------
// FSKitShell — public Rust surface
// ----------------------------------------------------------------

/// Adapter that exposes a [`ContentAddressedMount`] to the kernel
/// via FSKit. Owns the Swift-side session handle; `Drop` releases
/// it (which in turn drops the boxed [`PlatformShell`] on the Rust
/// side).
pub struct FSKitShell {
    handle: HeddleFSKitSessionHandle,
}

// SAFETY: The Swift session is dispatch-thread-safe. The contained
// `Arc<dyn PlatformShell>` (boxed inside `user_data`) implements
// the trait under interior mutability, so concurrent callbacks
// from FSKit are well-defined.
unsafe impl Send for FSKitShell {}
unsafe impl Sync for FSKitShell {}

impl FSKitShell {
    /// Wrap a mount into an FSKit session.
    pub fn new(mount: ContentAddressedMount) -> Self {
        Self::from_shell(Arc::new(mount))
    }

    /// Construct from any [`PlatformShell`]. Useful in tests where
    /// you want to wire a mock shell into the FSKit ABI without
    /// spinning up a real `ContentAddressedMount`.
    pub fn from_shell(shell: Arc<dyn PlatformShell + Send + Sync>) -> Self {
        // Box the shell once and leak the pointer into the C ABI.
        // The Swift session calls `drop_callback` exactly once when
        // the session is freed; that reclaims the box.
        let boxed: Box<Arc<dyn PlatformShell + Send + Sync>> = Box::new(shell);
        let user_data = Box::into_raw(boxed) as *mut c_void;

        let handle = unsafe {
            heddle_fskit_session_new(
                user_data,
                Some(trampoline_lookup),
                Some(trampoline_getattr),
                Some(trampoline_read),
                Some(trampoline_write),
                Some(trampoline_enumerate),
                Some(trampoline_flush),
                Some(trampoline_drop),
            )
        };
        if handle.is_null() {
            // Reclaim the box so we don't leak. `session_new`
            // returning null is a hard failure of the Swift shim,
            // not a runtime condition we expect.
            unsafe {
                drop(Box::from_raw(
                    user_data as *mut Arc<dyn PlatformShell + Send + Sync>,
                ))
            };
            panic!("heddle_fskit_session_new returned null");
        }
        Self { handle }
    }

    /// Returns true when the host OS can actually load the FSKit
    /// framework (macOS 15.4+). When this is false, `mount_background`
    /// will return an error rather than a usable session.
    pub fn is_runtime_available() -> bool {
        // SAFETY: pure Swift function, no preconditions.
        unsafe { heddle_fskit_is_available() == 1 }
    }

    /// Mount in a background session. Caller holds the returned
    /// [`FSKitSession`]; dropping it triggers an unmount.
    ///
    /// Currently the underlying Swift implementation returns
    /// `ENOSYS` because the `FSModuleHost.register` step is stubbed
    /// (see module-level docs). The constructor lifecycle and the
    /// callback wiring are exercised; the actual kernel publish is
    /// a follow-up.
    pub fn mount_background(self, mountpoint: impl AsRef<Path>) -> Result<FSKitSession> {
        let path = mountpoint.as_ref();
        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|e| MountError::Stale(format!("mountpoint contains NUL: {e}")))?;
        // Take ownership of the handle out of `self` so the `Drop`
        // for `FSKitShell` doesn't double-free when this scope ends
        // and ownership transfers to `FSKitSession`.
        let handle = self.handle;
        std::mem::forget(self);

        let rc = unsafe { heddle_fskit_session_mount(handle, c_path.as_ptr()) };
        if rc != 0 {
            // Mount failed — clean up the session ourselves since
            // the caller never sees the handle.
            unsafe { heddle_fskit_session_free(handle) };
            return Err(MountError::Store(objects::error::HeddleError::Io(
                std::io::Error::from_raw_os_error(rc),
            )));
        }
        Ok(FSKitSession {
            handle: Some(handle),
        })
    }
}

impl Drop for FSKitShell {
    fn drop(&mut self) {
        // Releases the session and (via the Swift `deinit`) fires
        // the Rust drop callback that reclaims the boxed shell.
        if !self.handle.is_null() {
            unsafe { heddle_fskit_session_free(self.handle) };
        }
    }
}

// ----------------------------------------------------------------
// FSKitSession — RAII unmount handle
// ----------------------------------------------------------------

/// Live FSKit mount session. Drop unmounts.
pub struct FSKitSession {
    // Option so explicit `unmount()` can drain it before Drop.
    handle: Option<HeddleFSKitSessionHandle>,
}

// SAFETY: see FSKitShell.
unsafe impl Send for FSKitSession {}
unsafe impl Sync for FSKitSession {}

impl FSKitSession {
    /// Force the mount to unmount immediately. Equivalent to
    /// dropping the session, but surfaces the unmount errno.
    pub fn unmount(mut self) -> Result<()> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        let rc = unsafe { heddle_fskit_session_unmount(handle) };
        let unmount_err = if rc != 0 {
            Some(MountError::Store(objects::error::HeddleError::Io(
                std::io::Error::from_raw_os_error(rc),
            )))
        } else {
            None
        };
        unsafe { heddle_fskit_session_free(handle) };
        match unmount_err {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

impl Drop for FSKitSession {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        let rc = unsafe { heddle_fskit_session_unmount(handle) };
        if rc != 0 {
            warn!(rc, "fskit session unmount returned non-zero on drop");
        }
        unsafe { heddle_fskit_session_free(handle) };
    }
}

// ----------------------------------------------------------------
// C-ABI trampolines: convert raw C arguments back into trait calls.
//
// All trampolines follow the same shape:
//   1. Reconstruct the `&Arc<dyn PlatformShell>` from `user_data`
//      *without* taking ownership (it's owned by the Swift session
//      until `trampoline_drop` fires).
//   2. Validate pointers; map malformed input to EINVAL.
//   3. Dispatch into the trait.
//   4. Translate the `Result` into an errno + outparam writes.
// ----------------------------------------------------------------

/// View the boxed shell without consuming ownership.
///
/// SAFETY: caller must guarantee `user_data` was the pointer
/// originally returned by `Box::into_raw` in `FSKitShell::from_shell`,
/// and that the box has not been dropped (it lives until
/// `trampoline_drop` is invoked by the Swift session's deinit).
#[inline]
unsafe fn shell_ref<'a>(
    user_data: *mut c_void,
) -> Option<&'a Arc<dyn PlatformShell + Send + Sync>> {
    if user_data.is_null() {
        return None;
    }
    // SAFETY: per the function's contract; the box pointer is
    // valid until `trampoline_drop` runs.
    Some(unsafe { &*(user_data as *const Arc<dyn PlatformShell + Send + Sync>) })
}

fn errno_from(err: MountError) -> c_int {
    err.to_errno()
}

unsafe extern "C" fn trampoline_lookup(
    user_data: *mut c_void,
    parent_inode: u64,
    name_utf8: *const c_char,
    out_child_inode: *mut u64,
    out_unix_mode: *mut u32,
    out_size: *mut u64,
) -> c_int {
    // SAFETY: every dereference below is delegated to a function
    // with a documented contract; the FSKit caller upholds those.
    unsafe {
        let Some(shell) = shell_ref(user_data) else {
            return libc::EINVAL;
        };
        if name_utf8.is_null() {
            return libc::EINVAL;
        }
        let cstr = CStr::from_ptr(name_utf8);
        let name: &OsStr = OsStr::from_bytes(cstr.to_bytes());

        match shell.lookup(NodeId(parent_inode), name) {
            Ok(Some(entry)) => {
                write_out(out_child_inode, entry.node.0);
                write_out(out_unix_mode, entry.unix_mode);
                write_out(out_size, entry.size);
                0
            }
            Ok(None) => libc::ENOENT,
            Err(err) => errno_from(err),
        }
    }
}

unsafe extern "C" fn trampoline_getattr(
    user_data: *mut c_void,
    inode: u64,
    out_unix_mode: *mut u32,
    out_size: *mut u64,
    out_nlink: *mut u32,
) -> c_int {
    // SAFETY: see `trampoline_lookup`.
    unsafe {
        let Some(shell) = shell_ref(user_data) else {
            return libc::EINVAL;
        };
        match shell.attrs(NodeId(inode)) {
            Ok(attrs) => {
                write_out(out_unix_mode, attrs.unix_mode);
                write_out(out_size, attrs.size);
                write_out(out_nlink, attrs.nlink);
                0
            }
            Err(err) => errno_from(err),
        }
    }
}

unsafe extern "C" fn trampoline_read(
    user_data: *mut c_void,
    inode: u64,
    offset: u64,
    buffer: *mut u8,
    buffer_capacity: u64,
    out_bytes_read: *mut u64,
) -> c_int {
    // SAFETY: caller (FSKit reply path) provides a buffer of
    // `buffer_capacity` bytes that lives until this call returns.
    unsafe {
        let Some(shell) = shell_ref(user_data) else {
            return libc::EINVAL;
        };
        if buffer.is_null() {
            return libc::EINVAL;
        }
        let cap = buffer_capacity as usize;
        let buf = std::slice::from_raw_parts_mut(buffer, cap);
        match shell.read(NodeId(inode), offset, buf) {
            Ok(n) => {
                write_out(out_bytes_read, n as u64);
                0
            }
            Err(err) => errno_from(err),
        }
    }
}

unsafe extern "C" fn trampoline_write(
    user_data: *mut c_void,
    inode: u64,
    offset: u64,
    data: *const u8,
    data_len: u64,
    out_bytes_written: *mut u64,
) -> c_int {
    // SAFETY: caller (FSKit write path) provides a `data_len`-byte
    // buffer that lives until this call returns.
    unsafe {
        let Some(shell) = shell_ref(user_data) else {
            return libc::EINVAL;
        };
        if data.is_null() && data_len > 0 {
            return libc::EINVAL;
        }
        let slice = if data_len == 0 {
            &[][..]
        } else {
            std::slice::from_raw_parts(data, data_len as usize)
        };
        match shell.write(NodeId(inode), offset, slice) {
            Ok(n) => {
                write_out(out_bytes_written, n as u64);
                0
            }
            Err(err) => errno_from(err),
        }
    }
}

unsafe extern "C" fn trampoline_enumerate(
    user_data: *mut c_void,
    dir_inode: u64,
    emit_user_data: *mut c_void,
    emit: HeddleEnumerateEmit,
) -> c_int {
    // SAFETY: `shell_ref` and `emit` invocations both rely on the
    // FSKit-side ownership contract documented at the top of this
    // module.
    unsafe {
        let Some(shell) = shell_ref(user_data) else {
            return libc::EINVAL;
        };
        let Some(emit) = emit else {
            return libc::EINVAL;
        };
        let entries: Vec<Entry> = match shell.enumerate(NodeId(dir_inode)) {
            Ok(e) => e,
            Err(err) => return errno_from(err),
        };
        for entry in entries {
            // Convert the OS string to a NUL-terminated C string.
            // `OsString` may contain a NUL on bizarre filesystems,
            // but any path Heddle can serve was constructed from
            // valid UTF-8 tree entries — fail loudly if that ever
            // changes.
            let bytes = entry.name.as_os_str().as_bytes();
            let Ok(c_name) = CString::new(bytes) else {
                warn!(?entry.name, "fskit enumerate: skipping entry with embedded NUL");
                continue;
            };
            let rc = emit(
                emit_user_data,
                entry.node.0,
                c_name.as_ptr(),
                entry.unix_mode,
                entry.size,
            );
            if rc != 0 {
                // Swift signalled "stop" (typically because its
                // reply buffer is full). Not an error; the kernel
                // will call back to resume.
                break;
            }
        }
        0
    }
}

unsafe extern "C" fn trampoline_flush(user_data: *mut c_void, inode: u64) -> c_int {
    // SAFETY: see `trampoline_lookup`.
    unsafe {
        let Some(shell) = shell_ref(user_data) else {
            return libc::EINVAL;
        };
        match shell.flush(NodeId(inode)) {
            Ok(()) => 0,
            Err(err) => errno_from(err),
        }
    }
}

/// Reclaim the boxed shell. Called by the Swift session's deinit.
unsafe extern "C" fn trampoline_drop(user_data: *mut c_void) {
    if user_data.is_null() {
        return;
    }
    // SAFETY: `user_data` was produced by `Box::into_raw` in
    // `FSKitShell::from_shell` and is dropped exactly once here.
    unsafe {
        drop(Box::from_raw(
            user_data as *mut Arc<dyn PlatformShell + Send + Sync>,
        ));
    }
}

#[inline]
fn write_out<T: Copy>(ptr: *mut T, value: T) {
    if !ptr.is_null() {
        // SAFETY: the Swift caller pre-validates these pointers
        // (allocates stack slots before invoking the callback).
        unsafe { ptr.write(value) };
    }
}

// Suppress "unused" lints when the module compiles without ever
// constructing an `Attrs` (used by the test mock below). Not needed
// in production paths — kept as a no-op import gate.
#[allow(dead_code)]
fn _attrs_in_scope_check() -> Option<(Attrs, NodeKind, SystemTime, u32)> {
    let _ = DIR_UNIX_MODE;
    None
}

// ----------------------------------------------------------------
// Tests
// ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::UNIX_EPOCH,
    };

    use super::*;

    /// Trivial in-memory shell that lets us validate the FSKit
    /// session construct-and-drop lifecycle without needing a real
    /// `ContentAddressedMount` (which requires a Repository).
    struct CountingShell {
        drops: Arc<AtomicUsize>,
    }

    impl Drop for CountingShell {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl PlatformShell for CountingShell {
        fn lookup(&self, _parent: NodeId, _name: &OsStr) -> Result<Option<Entry>> {
            Ok(None)
        }
        fn read(&self, _node: NodeId, _offset: u64, _buf: &mut [u8]) -> Result<usize> {
            Ok(0)
        }
        fn write(&self, _node: NodeId, _offset: u64, _data: &[u8]) -> Result<usize> {
            Err(MountError::ReadOnly)
        }
        fn enumerate(&self, _dir: NodeId) -> Result<Vec<Entry>> {
            Ok(vec![])
        }
        fn attrs(&self, node: NodeId) -> Result<Attrs> {
            Ok(Attrs {
                node,
                kind: NodeKind::Directory,
                size: 0,
                unix_mode: DIR_UNIX_MODE,
                nlink: 2,
                mtime: UNIX_EPOCH,
            })
        }
        fn invalidate(&self, _node: NodeId) -> Result<()> {
            Ok(())
        }
    }

    /// Construct an FSKitShell, drop it, and verify the drop
    /// callback fired (releasing the boxed shell exactly once).
    ///
    /// Marked `#[ignore]` because the Swift-side static lib is only
    /// linked when the `fskit` feature is on; running this without
    /// `--features fskit` will produce a link error at test time.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires the fskit feature; opt-in via --features fskit"]
    fn fskit_session_lifecycle_drops_inner_shell() {
        let drops = Arc::new(AtomicUsize::new(0));
        let shell = Arc::new(CountingShell {
            drops: Arc::clone(&drops),
        });
        let fskit = FSKitShell::from_shell(shell);
        // Construction alone shouldn't drop the inner shell.
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(fskit);
        // After dropping the FSKitShell, the Swift `deinit` fires
        // `trampoline_drop`, which reclaims the box and runs the
        // CountingShell destructor exactly once.
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    /// Sanity: the runtime-availability probe is callable and
    /// returns a bool. We can't assert true/false because the
    /// dev box may or may not be on macOS 15.4.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires the fskit feature; opt-in via --features fskit"]
    fn fskit_runtime_availability_is_callable() {
        let _ = FSKitShell::is_runtime_available();
    }

    /// Drift sentinel: the cbindgen-generated bridging header that
    /// Swift consumes must declare every Rust-side C-ABI symbol.
    /// This test reads the on-disk header and asserts each typedef
    /// and prototype is present.
    ///
    /// Why this matters: `build.rs` regenerates the header on every
    /// `cargo build --features fskit`, so a `cargo test --features
    /// fskit` always sees a fresh header. If a developer renames or
    /// removes a Rust C-ABI item without updating Swift, this test
    /// fails *before* the Swift static lib is exercised at runtime —
    /// turning the silent-UB case the prior hand-written header
    /// could produce into a loud test failure.
    ///
    /// Adding a new C-ABI symbol? Append it to `EXPECTED_SYMBOLS`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires the fskit feature; opt-in via --features fskit"]
    fn fskit_bridging_header_declares_every_c_abi_symbol() {
        const EXPECTED_SYMBOLS: &[&str] = &[
            // Opaque handle.
            "HeddleFSKitSessionHandle",
            // Callback typedefs.
            "HeddleLookupCallback",
            "HeddleGetattrCallback",
            "HeddleReadCallback",
            "HeddleWriteCallback",
            "HeddleEnumerateEmit",
            "HeddleEnumerateCallback",
            "HeddleFlushCallback",
            "HeddleDropCallback",
            // Function prototypes.
            "heddle_fskit_session_new",
            "heddle_fskit_session_mount",
            "heddle_fskit_session_unmount",
            "heddle_fskit_session_free",
            "heddle_fskit_is_available",
        ];

        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("swift")
            .join("HeddleFSKit")
            .join("HeddleFSKit-Bridging.h");
        let header = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

        // The cbindgen banner is required so reviewers don't try to
        // hand-edit the file. If this drifts, regenerate via
        // `cargo build -p mount --features fskit`.
        assert!(
            header.contains("GENERATED FROM `crates/mount/src/fskit/c_abi.rs`"),
            "bridging header missing the cbindgen 'GENERATED' banner; \
             rerun `cargo build -p mount --features fskit`"
        );

        for sym in EXPECTED_SYMBOLS {
            assert!(
                header.contains(sym),
                "bridging header missing `{sym}` — has the C ABI in \
                 src/fskit/c_abi.rs drifted? Re-run `cargo build -p \
                 mount --features fskit` to regenerate."
            );
        }
    }
}