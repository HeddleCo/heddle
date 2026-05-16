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
    ffi::{c_char, c_int, c_void, CStr, CString, OsStr},
    os::unix::ffi::OsStrExt,
    path::Path,
    sync::Arc,
};

use tracing::warn;

use crate::{
    core::ContentAddressedMount,
    error::{MountError, Result},
    shell::{Entry, NodeId, PlatformShell},
};

pub mod c_abi;
pub mod readiness;

// The C ABI is declared *once*, in [`c_abi`]. The Swift bridging
// header (`swift/HeddleFSKit/HeddleFSKit-Bridging.h`) is regenerated
// from that module by `cbindgen` at build time. Use these aliases
// throughout this file so any signature change in `c_abi` propagates
// here at the type-checker, not at runtime.
use c_abi::{
    heddle_fskit_is_available, heddle_fskit_session_free, heddle_fskit_session_mount,
    heddle_fskit_session_new, heddle_fskit_session_unmount, HeddleEnumerateEmit,
    HeddleFSKitSessionHandle,
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

    /// Consume the shell and return the raw session handle.
    ///
    /// The caller becomes responsible for releasing the handle via
    /// [`heddle_fskit_session_free`]. Used by the System Extension
    /// bootstrap path ([`heddle_fskit_open_thread`]) where the
    /// handle is handed to Swift code that manages the lifetime
    /// rather than wrapping it in a Rust RAII type.
    pub fn into_handle(self) -> HeddleFSKitSessionHandle {
        let handle = self.handle;
        std::mem::forget(self);
        handle
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
// open_thread — implementation backing `c_abi::heddle_fskit_open_thread`
// ----------------------------------------------------------------

/// Open a content-addressed mount for `thread_id` at `repo_path`
/// and return a raw FSKit session handle. Returns a null handle on
/// any failure; the C ABI wrapper logs the details.
///
/// The caller (Swift, via `c_abi::heddle_fskit_open_thread`) owns
/// the returned handle and must call
/// [`c_abi::heddle_fskit_session_free`] when the volume is
/// destroyed.
/// File-based logger for the extension. macOS doesn't route
/// stderr from ExtensionKit extensions to os_log, and the Rust
/// `tracing` crate has no subscriber in this context — so we
/// append to a known file the user can `cat` after a mount
/// attempt. `/tmp/heddle-fskit.log` is covered by our absolute-
/// path temp-exception entitlement.
fn fskit_log(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/heddle-fskit.log")
    {
        let _ = writeln!(
            f,
            "[{}] {}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            msg
        );
    }
}

pub(super) fn open_thread(repo_path: &str, thread_id: &str) -> c_abi::HeddleFSKitSessionHandle {
    fskit_log(&format!("open_thread: repo={repo_path} thread={thread_id}"));
    let repo = match repo::Repository::open(repo_path) {
        Ok(r) => {
            fskit_log("Repository::open succeeded");
            r
        }
        Err(e) => {
            fskit_log(&format!("Repository::open FAILED: {e:?}"));
            return std::ptr::null_mut();
        }
    };
    let mount = match ContentAddressedMount::new(repo, thread_id) {
        Ok(m) => {
            fskit_log("ContentAddressedMount::new succeeded");
            m
        }
        Err(e) => {
            fskit_log(&format!("ContentAddressedMount::new FAILED: {e:?}"));
            return std::ptr::null_mut();
        }
    };
    fskit_log("open_thread returning handle");
    FSKitShell::new(mount).into_handle()
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

/// Catch any panic inside an FSKit trampoline and convert it to
/// `EIO` instead of letting the unwind cross the C ABI boundary.
///
/// Rust ≥1.81 converts unwind-across-`extern "C"` to abort, so a
/// panic in a trampoline body would otherwise crash the entire
/// System Extension process — and with it every materialised
/// FSKit volume hosted by `heddled`. A single poisoned mutex or
/// unwrap-deep-in-a-shell-call now produces an `EIO` for the one
/// operation that hit the bad path; the kernel surfaces it to the
/// userland reader as a normal I/O failure and the rest of the
/// volume keeps serving. The panic gets logged in `tracing` first
/// so the operator can see what actually broke.
///
/// `AssertUnwindSafe` is correct here: the trampolines own no
/// state across the catch boundary, and any references they hold
/// (the shell `Arc`, the FFI outparams) are written through raw
/// pointers — outparams won't be torn by a panicking writer
/// because we don't write them on the error path.
#[inline]
fn guarded_c_int<F: FnOnce() -> c_int>(label: &'static str, f: F) -> c_int {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(rc) => rc,
        Err(payload) => {
            let msg = panic_payload_str(&payload);
            tracing::error!(trampoline = label, %msg, "FSKit trampoline panicked; returning EIO");
            libc::EIO
        }
    }
}

/// `void`-returning variant for `trampoline_drop`. Returning `()`
/// is the same shape as a successful errno-0 trampoline so the
/// helper boils down to "log and swallow". Dropping the boxed
/// shell shouldn't panic in practice — `Arc` drop is straight-line
/// — but we hold the line anyway: a panic during deinit would
/// otherwise abort `heddled` itself.
#[inline]
fn guarded_drop<F: FnOnce()>(label: &'static str, f: F) {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        let msg = panic_payload_str(&payload);
        tracing::error!(trampoline = label, %msg, "FSKit trampoline panicked during drop");
    }
}

fn panic_payload_str(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

unsafe extern "C" fn trampoline_lookup(
    user_data: *mut c_void,
    parent_inode: u64,
    name_utf8: *const c_char,
    out_child_inode: *mut u64,
    out_unix_mode: *mut u32,
    out_size: *mut u64,
) -> c_int {
    guarded_c_int("lookup", || {
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
    })
}

unsafe extern "C" fn trampoline_getattr(
    user_data: *mut c_void,
    inode: u64,
    out_unix_mode: *mut u32,
    out_size: *mut u64,
    out_nlink: *mut u32,
    out_mtime_sec: *mut i64,
) -> c_int {
    guarded_c_int("getattr", || {
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
                    write_out(out_mtime_sec, mtime_to_secs(attrs.mtime));
                    0
                }
                Err(err) => errno_from(err),
            }
        }
    })
}

/// Convert a `SystemTime` to seconds-since-UNIX-epoch. Pre-epoch
/// times collapse to 0 (the mount's bootstrap clock is post-2025).
fn mtime_to_secs(t: std::time::SystemTime) -> i64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

unsafe extern "C" fn trampoline_read(
    user_data: *mut c_void,
    inode: u64,
    offset: u64,
    buffer: *mut u8,
    buffer_capacity: u64,
    out_bytes_read: *mut u64,
) -> c_int {
    guarded_c_int("read", || {
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
    })
}

unsafe extern "C" fn trampoline_write(
    user_data: *mut c_void,
    inode: u64,
    offset: u64,
    data: *const u8,
    data_len: u64,
    out_bytes_written: *mut u64,
) -> c_int {
    guarded_c_int("write", || {
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
    })
}

unsafe extern "C" fn trampoline_enumerate(
    user_data: *mut c_void,
    dir_inode: u64,
    emit_user_data: *mut c_void,
    emit: HeddleEnumerateEmit,
) -> c_int {
    guarded_c_int("enumerate", || {
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
            // Resolve the mount's mtime once per directory listing
            // rather than per entry. `ContentAddressedMount` returns
            // `mounted_at` for every node, so a single `attrs` call on
            // the parent directory is representative; if it fails we
            // fall back to epoch (same as the prior placeholder).
            let dir_mtime_sec = shell
                .attrs(NodeId(dir_inode))
                .ok()
                .map(|a| mtime_to_secs(a.mtime))
                .unwrap_or(0);
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
                    dir_mtime_sec,
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
    })
}

unsafe extern "C" fn trampoline_flush(user_data: *mut c_void, inode: u64) -> c_int {
    guarded_c_int("flush", || {
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
    })
}

/// Reclaim the boxed shell. Called by the Swift session's deinit.
unsafe extern "C" fn trampoline_drop(user_data: *mut c_void) {
    if user_data.is_null() {
        return;
    }
    guarded_drop("drop", || {
        // SAFETY: `user_data` was produced by `Box::into_raw` in
        // `FSKitShell::from_shell` and is dropped exactly once here.
        unsafe {
            drop(Box::from_raw(
                user_data as *mut Arc<dyn PlatformShell + Send + Sync>,
            ));
        }
    });
}

#[inline]
fn write_out<T: Copy>(ptr: *mut T, value: T) {
    if !ptr.is_null() {
        // SAFETY: the Swift caller pre-validates these pointers
        // (allocates stack slots before invoking the callback).
        unsafe { ptr.write(value) };
    }
}

// ----------------------------------------------------------------
// Tests
// ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::tests::mocks::CountingShell;

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
        let (counting, drops) = CountingShell::new();
        let shell = Arc::new(counting);
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
            "heddle_fskit_open_thread",
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

    /// FFI panic-resilience: a panic deep in a `PlatformShell` call
    /// must be caught by the trampoline's `guarded_c_int` wrapper
    /// and converted to `EIO`, not allowed to unwind across the
    /// `extern "C"` boundary (which Rust ≥1.81 converts to abort,
    /// taking the whole System Extension process — and every
    /// materialised volume — with it).
    ///
    /// Driving the trampoline directly with a boxed `PanicShell`
    /// exercises exactly the unwind path. No Swift static lib
    /// needed: the trampolines are plain Rust functions reachable
    /// from the test binary regardless of `--features fskit`.
    ///
    /// `Box::into_raw` mirrors the production setup in
    /// `FSKitShell::from_shell`, so the test also exercises the
    /// pointer-shape contract that `shell_ref` decodes.
    #[test]
    fn trampoline_lookup_recovers_eio_on_panic() {
        use crate::tests::mocks::PanicShell;

        // Box::into_raw a fat-pointer Arc the same way the
        // production constructor does. `Arc<dyn PlatformShell>` is
        // what the trampolines downcast back to via `shell_ref`.
        let shell: Arc<dyn PlatformShell + Send + Sync> = Arc::new(PanicShell);
        let boxed: Box<Arc<dyn PlatformShell + Send + Sync>> = Box::new(shell);
        let user_data = Box::into_raw(boxed) as *mut c_void;

        // Outparams ProjFS-style — stack slots the trampoline may
        // or may not write to. Default-zeroed so we can also assert
        // the trampoline didn't tear them on the error path.
        let mut child_inode: u64 = 0;
        let mut unix_mode: u32 = 0;
        let mut size: u64 = 0;
        let name = std::ffi::CString::new("anything").unwrap();

        // SAFETY: `user_data` is the exact pointer shape
        // `trampoline_lookup` expects (Box<Arc<dyn …>>), the name
        // is a NUL-terminated C string owned by this stack frame,
        // and the outparam pointers live until this call returns.
        let rc = unsafe {
            trampoline_lookup(
                user_data,
                /* parent_inode */ 1,
                name.as_ptr(),
                &mut child_inode as *mut u64,
                &mut unix_mode as *mut u32,
                &mut size as *mut u64,
            )
        };

        assert_eq!(
            rc,
            libc::EIO,
            "panic in PlatformShell::lookup must surface as EIO, \
             not propagate across the C ABI (got rc={rc})"
        );
        // Outparams stayed zero — the trampoline must not have
        // partially written through them on the error path.
        assert_eq!(child_inode, 0, "child_inode must not be torn on error");
        assert_eq!(unix_mode, 0, "unix_mode must not be torn on error");
        assert_eq!(size, 0, "size must not be torn on error");

        // Reclaim the box via the same trampoline production uses
        // for cleanup. A panic in this body would compound the
        // test failure, but `guarded_drop` should also catch.
        // SAFETY: `user_data` was produced by `Box::into_raw` above
        // and is reclaimed exactly once here.
        unsafe { trampoline_drop(user_data) };
    }
}
