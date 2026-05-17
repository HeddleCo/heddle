// SPDX-License-Identifier: Apache-2.0
//! Windows ProjFS shell.
//!
//! [`ProjFsShell`] mirrors [`crate::fuse::FuseShell`] for Windows 10
//! 1809+ / Server 2019+. The kernel-side adapter is the Projected
//! File System (`ProjectedFSLib.dll`, surfaced through the `windows`
//! crate's `Win32::Storage::ProjectedFileSystem` module).
//!
//! ## Layout
//!
//! ```text
//!   ProjFS  (kernel-side, NTFS-backed)
//!         ▲
//!         │  PRJ_CALLBACKS function pointers
//!         │
//!   ProjFsShell  ← this file
//!         ▲
//!         │  PlatformShell trait
//!         │
//!   ContentAddressedMount  ← pure Rust core, shared with FuseShell / FSKitShell
//! ```
//!
//! ## Write model mismatch (see `shell.rs` Platform notes)
//!
//! ProjFS does *not* deliver per-write callbacks. After a virtualized
//! file is hydrated by the first read, subsequent writes go straight
//! to the NTFS-backed file under the virtualization root and ProjFS
//! only notifies the provider when the handle closes
//! (`PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_MODIFIED`). We bridge
//! the mismatch inside [`notification_trampoline`]: on close-modified,
//! the trampoline reads the now-fully-hydrated file from NTFS and
//! synthesizes a single `write(node, 0, full)` + `flush(node)` against
//! the shell. Cost: one redundant memory copy per modified file.
//!
//! ## Runtime requirements
//!
//! * Windows 10 1809+ or Windows Server 2019+.
//! * The "Projected File System" Windows optional feature must be
//!   installed (`Enable-WindowsOptionalFeature -Online -FeatureName
//!   Client-ProjFS`). [`ProjFsShell::is_runtime_available`] probes
//!   for `ProjectedFSLib.dll` and reports failure when missing.
//! * The virtualization root must live on NTFS — `%USERPROFILE%`
//!   paths are always NTFS on a standard install.

use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use tracing::warn;
use windows::{
    core::{GUID, HRESULT, PCWSTR},
    Win32::{
        Foundation::{FreeLibrary, ERROR_INSUFFICIENT_BUFFER, S_OK},
        Storage::ProjectedFileSystem::{
            PrjAllocateAlignedBuffer, PrjFillDirEntryBuffer, PrjFreeAlignedBuffer,
            PrjMarkDirectoryAsPlaceholder, PrjStartVirtualizing, PrjStopVirtualizing,
            PrjWriteFileData, PrjWritePlaceholderInfo, PRJ_CALLBACKS, PRJ_CALLBACK_DATA,
            PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN, PRJ_DIR_ENTRY_BUFFER_HANDLE, PRJ_FILE_BASIC_INFO,
            PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT, PRJ_NOTIFICATION,
            PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_DELETED,
            PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_MODIFIED,
            PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_NO_MODIFICATION, PRJ_NOTIFICATION_FILE_RENAMED,
            PRJ_NOTIFICATION_MAPPING, PRJ_NOTIFY_TYPES, PRJ_PLACEHOLDER_INFO,
            PRJ_STARTVIRTUALIZING_OPTIONS,
        },
        System::LibraryLoader::LoadLibraryW,
    },
};

use crate::{
    core::ContentAddressedMount,
    error::{MountError, Result},
    shell::{Entry, NodeId, NodeKind, PlatformShell},
};

// ----------------------------------------------------------------
// ProjFsShell — public Rust surface
// ----------------------------------------------------------------

/// Adapter that exposes a [`ContentAddressedMount`] to the OS via
/// ProjFS. Owns the boxed shell in an `Arc` so the ProjFS worker
/// threads share the same registry; the box is reclaimed when the
/// matching [`ProjFsSession`] is dropped.
pub struct ProjFsShell {
    inner: Arc<dyn PlatformShell + Send + Sync>,
}

impl ProjFsShell {
    /// Wrap a mount into a ProjFS shell.
    pub fn new(mount: ContentAddressedMount) -> Self {
        Self::from_shell(Arc::new(mount))
    }

    /// Construct from any [`PlatformShell`]. Useful in tests where
    /// you want to wire a mock shell into the ProjFS callback ABI
    /// without spinning up a real `ContentAddressedMount`.
    pub fn from_shell(shell: Arc<dyn PlatformShell + Send + Sync>) -> Self {
        Self { inner: shell }
    }

    /// Returns true when the host has ProjFS available (`ProjectedFSLib.dll`
    /// loads cleanly). False on a Windows install that does not have
    /// the "Projected File System" optional feature enabled, and false
    /// on every non-Windows host (where this method is `cfg`-gated
    /// out at the module level anyway).
    pub fn is_runtime_available() -> bool {
        // SAFETY: pure DLL probe; on failure the OS sets last-error
        // but no other side effects. We free the handle immediately
        // if the load succeeded.
        unsafe {
            let dll = encode_wide("ProjectedFSLib.dll");
            match LoadLibraryW(PCWSTR(dll.as_ptr())) {
                Ok(handle) if !handle.is_invalid() => {
                    let _ = FreeLibrary(handle);
                    true
                }
                _ => false,
            }
        }
    }

    /// Mount in a background ProjFS instance. Caller holds the
    /// returned [`ProjFsSession`]; dropping it triggers
    /// `PrjStopVirtualizing` and reclaims the boxed shell.
    pub fn mount_background(self, virtualization_root: impl AsRef<Path>) -> Result<ProjFsSession> {
        let root = virtualization_root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)
            .map_err(|e| MountError::Store(objects::error::HeddleError::Io(e)))?;

        // ProjFS requires each virtualization instance to have a
        // stable GUID stamped into the directory's reparse metadata.
        // Persist it next to the root so re-mounting reuses the same
        // identity.
        let instance_id = load_or_create_instance_id(&root)?;

        // Mark the directory as a placeholder. Idempotent on a
        // directory that already carries the metadata — ProjFS
        // returns S_OK in that case. We pass an empty target-path
        // because the projection is fully callback-driven.
        // SAFETY: `root_wide` lives until after the call; no other
        // pointer aliasing.
        let root_wide = encode_wide(root.to_string_lossy().as_ref());
        unsafe {
            PrjMarkDirectoryAsPlaceholder(
                PCWSTR(root_wide.as_ptr()),
                PCWSTR::null(),
                None,
                &instance_id,
            )
            .map_err(|e| hresult_to_mount_error(e.code()))?;
        }

        // Box the shell *and* the virtualization root together and
        // leak the pointer into the ProjFS InstanceContext. The
        // session's Drop reclaims it.
        //
        // Bundling the root in is what makes the close-modified
        // notification trampoline correct: ProjFS hands us a path
        // relative to the virtualization root but does not give us
        // the root itself on every API version, so the trampoline
        // needs a stable source for "where on NTFS is this file
        // actually living?". Pre-fix the trampoline reached for
        // `std::env::current_dir()` — wrong as soon as the host
        // process chdir'd elsewhere; the kernel still delivered the
        // notification (because it's the *file* that closed, not the
        // process) but the read-back-from-NTFS step landed at a
        // bogus path and the edit silently never made it back into
        // the CAS.
        let context = Box::new(InstanceContext {
            shell: self.inner,
            virtualization_root: root.clone(),
            enumerations: Mutex::new(HashMap::new()),
        });
        let instance_context = Box::into_raw(context) as *const std::ffi::c_void;

        let callbacks = PRJ_CALLBACKS {
            StartDirectoryEnumerationCallback: Some(start_dir_enum_trampoline),
            EndDirectoryEnumerationCallback: Some(end_dir_enum_trampoline),
            GetDirectoryEnumerationCallback: Some(get_dir_enum_trampoline),
            GetPlaceholderInfoCallback: Some(get_placeholder_info_trampoline),
            GetFileDataCallback: Some(get_file_data_trampoline),
            QueryFileNameCallback: None,
            NotificationCallback: Some(notification_trampoline),
            CancelCommandCallback: None,
        };

        // The notification mapping subscribes us to close-modified,
        // close-deleted, and rename events. Reads (placeholder hydra-
        // tion) work without an explicit subscription.
        //
        // windows-rs 0.58 separates `PRJ_NOTIFICATION` (the enum
        // delivered to the trampoline) from `PRJ_NOTIFY_TYPES` (the
        // bitmask we ship into `PRJ_NOTIFICATION_MAPPING`); the two
        // have identical underlying `i32` storage but the type system
        // refuses the implicit conversion.
        // windows-rs 0.58 splits the notification surface in two:
        // the individual constants are typed `PRJ_NOTIFICATION`
        // (inner `i32`, used for the value delivered to the
        // notification trampoline) while the mask field on
        // `PRJ_NOTIFICATION_MAPPING` is `PRJ_NOTIFY_TYPES` (inner
        // `u32`). The constants' bit values are identical between
        // the two; we OR them as `i32` and re-cast on the way into
        // the mask. The transmute-via-cast is sound because both
        // types are `#[repr(transparent)]` over the underlying
        // 32-bit integer with the same bit-layout.
        let notification_bits = PRJ_NOTIFY_TYPES(
            (PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_MODIFIED.0
                | PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_DELETED.0
                | PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_NO_MODIFICATION.0
                | PRJ_NOTIFICATION_FILE_RENAMED.0) as u32,
        );
        let mut notification_mapping = entire_root_notification_mapping(notification_bits);

        let options = PRJ_STARTVIRTUALIZING_OPTIONS {
            Flags: Default::default(),
            PoolThreadCount: 0,
            ConcurrentThreadCount: 0,
            NotificationMappings: &mut notification_mapping,
            NotificationMappingsCount: 1,
        };

        // SAFETY: `root_wide`, `callbacks`, and `options` outlive the
        // call. `instance_context` is the only thing that has to
        // outlive *the mount*, and it does — it's held by the kernel
        // and reclaimed in our Drop.
        //
        // windows-rs 0.58 returns the handle from `PrjStartVirtualizing`
        // rather than taking it as an out parameter (the older shape
        // the scaffold targeted).
        let handle = match unsafe {
            PrjStartVirtualizing(
                PCWSTR(root_wide.as_ptr()),
                &callbacks,
                Some(instance_context),
                Some(&options),
            )
        } {
            Ok(h) => h,
            Err(e) => {
                // Mount didn't start; reclaim the box so we don't leak.
                // SAFETY: `instance_context` was produced by Box::into_raw
                // above and the kernel never took ownership.
                unsafe {
                    drop(Box::from_raw(instance_context as *mut InstanceContext));
                }
                return Err(hresult_to_mount_error(e.code()));
            }
        };

        Ok(ProjFsSession {
            handle: Some(handle),
            instance_context,
            virtualization_root: root,
        })
    }
}

// ----------------------------------------------------------------
// ProjFsSession — RAII unmount handle
// ----------------------------------------------------------------

/// Live ProjFS virtualization instance. Drop stops virtualization.
pub struct ProjFsSession {
    handle: Option<PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT>,
    /// Pointer to the boxed `Arc<dyn PlatformShell>` that ProjFS
    /// holds as its `InstanceContext`. Reclaimed in Drop after
    /// `PrjStopVirtualizing` has returned (which guarantees no
    /// callback is in flight).
    instance_context: *const std::ffi::c_void,
    virtualization_root: PathBuf,
}

// SAFETY: the boxed shell behind `instance_context` is `Send + Sync`;
// the `PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT` handle is dispatch-thread
// -safe per ProjFS contract.
unsafe impl Send for ProjFsSession {}
unsafe impl Sync for ProjFsSession {}

impl ProjFsSession {
    /// Force virtualization to stop immediately. Equivalent to
    /// dropping the session, but surfaces the HRESULT.
    pub fn unmount(mut self) -> Result<()> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        // SAFETY: `handle` was produced by `PrjStartVirtualizing` and
        // has not been stopped yet (the take() above is the only
        // place that could clear it).
        unsafe { PrjStopVirtualizing(handle) };
        self.reclaim_context();
        Ok(())
    }

    pub fn virtualization_root(&self) -> &Path {
        &self.virtualization_root
    }

    fn reclaim_context(&mut self) {
        if self.instance_context.is_null() {
            return;
        }
        // SAFETY: `instance_context` was produced by Box::into_raw in
        // `mount_background` and is reclaimed exactly once here.
        // `PrjStopVirtualizing` has returned by this point, so no
        // callback is in flight.
        unsafe {
            drop(Box::from_raw(self.instance_context as *mut InstanceContext));
        }
        self.instance_context = std::ptr::null();
    }
}

/// Per-mount state the ProjFS trampolines need access to. Boxed and
/// handed to the kernel as the `InstanceContext` pointer; pulled back
/// out on each callback. Holds the shell pointer (the same `Arc` we
/// would have boxed naked before) plus the virtualization root path
/// so the close-modified bridge can read the hydrated NTFS file at
/// `virtualization_root.join(rel_path)` regardless of the host
/// process's current working directory.
///
/// `enumerations` is the per-directory-enumeration cursor cache,
/// keyed by the `enumeration_id` GUID the kernel hands us across the
/// start/get/end trio. ProjFS does not cache the listing for us; if
/// the user-side buffer is too small to fit every entry in one
/// `get_dir_enum` call, the kernel will recall us and expect us to
/// resume from where the previous call left off. Pre-fix the trampoline
/// re-enumerated from index 0 on every recall, which (a) duplicated
/// every entry in directories with > kernel-buffer worth of children
/// (~32 entries on a typical 8KiB buffer), and (b) prevented the
/// kernel from ever seeing `EntriesAvailable=false`, so listings of
/// large directories looped forever. The cursor here is the entry
/// index of the next-to-send file.
struct InstanceContext {
    shell: Arc<dyn PlatformShell + Send + Sync>,
    virtualization_root: PathBuf,
    enumerations: Mutex<HashMap<EnumKey, EnumState>>,
}

/// Hashable wrapper around `GUID` so we can key the per-enumeration
/// cursor map by the raw `enumeration_id` ProjFS hands us. `GUID`
/// itself is `Copy + Eq` but does not implement `Hash`, so we
/// canonicalise to the little-endian byte form (matching the format
/// the on-disk `.heddle-projfs-id` sidecar uses).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct EnumKey([u8; 16]);

impl EnumKey {
    fn from_guid(g: &GUID) -> Self {
        let mut buf = [0u8; 16];
        buf[0..4].copy_from_slice(&g.data1.to_le_bytes());
        buf[4..6].copy_from_slice(&g.data2.to_le_bytes());
        buf[6..8].copy_from_slice(&g.data3.to_le_bytes());
        buf[8..16].copy_from_slice(&g.data4);
        Self(buf)
    }
}

/// One per active `enumeration_id`. `entries` is the lazily-populated
/// snapshot from `PlatformShell::enumerate` (populated on the first
/// `get_dir_enum` call after `start_dir_enum`, and again whenever the
/// kernel sets `PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN`). `cursor` is the
/// index of the next entry to emit.
struct EnumState {
    entries: Vec<Entry>,
    cursor: usize,
    populated: bool,
}

impl EnumState {
    fn empty() -> Self {
        Self {
            entries: Vec::new(),
            cursor: 0,
            populated: false,
        }
    }

    fn reset(&mut self) {
        self.entries.clear();
        self.cursor = 0;
        self.populated = false;
    }
}

impl Drop for ProjFsSession {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            // SAFETY: see `unmount`.
            unsafe { PrjStopVirtualizing(handle) };
        }
        self.reclaim_context();
    }
}

// ----------------------------------------------------------------
// Helpers
// ----------------------------------------------------------------

/// Encode a UTF-8 path/string into a NUL-terminated UTF-16 vector
/// suitable for handing to a `PCWSTR`.
fn encode_wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Decode a `PCWSTR` back into a Rust `OsString` for `lookup`/`enumerate`
/// dispatch. ProjFS hands us file names as UTF-16; the trait wants
/// `&OsStr`.
unsafe fn decode_wide(ptr: PCWSTR) -> OsString {
    if ptr.is_null() {
        return OsString::new();
    }
    let mut len = 0usize;
    // SAFETY: the caller guarantees `ptr` points to a NUL-terminated
    // UTF-16 sequence owned by ProjFS for the lifetime of the
    // callback.
    while unsafe { *ptr.0.add(len) } != 0 {
        len += 1;
    }
    let slice = unsafe { std::slice::from_raw_parts(ptr.0, len) };
    OsString::from_wide(slice)
}

/// Walk a forward-slash- or backslash-separated relative path from
/// the mount root, returning the resolved `NodeId`. Empty path → ROOT.
fn resolve_path(shell: &Arc<dyn PlatformShell + Send + Sync>, relative: &Path) -> Result<NodeId> {
    let mut node = NodeId::ROOT;
    for component in relative.components() {
        let name = component.as_os_str();
        if name.is_empty() {
            continue;
        }
        let entry = shell
            .lookup(node, name)?
            .ok_or_else(|| MountError::NotFound(relative.display().to_string()))?;
        node = entry.node;
    }
    Ok(node)
}

/// Persist a per-virtualization-root GUID so reopens reuse the same
/// instance identity.
///
/// Two-tier sidecar location, chosen at runtime based on which
/// location is writable:
///
/// 1. **Primary**: `<parent>/.<basename>.heddle-projfs-id` — sidecar
///    lives in the parent directory of the virtualization root. The
///    file is outside the projection envelope, invisible to
///    `dir`/`Get-ChildItem`/`fs::read_dir` of the mountpoint, and
///    survives remounts of the same path. This is the default and
///    matches the common case where the parent is the same
///    user-writable scratch area the mount root sits in.
/// 2. **Fallback**: `<root>/.heddle-projfs-id` — sidecar inside the
///    mount root. Used when the primary path can't be written
///    (parent-directory ACL refuses us, parent is read-only, the
///    process runs as a service account with write access only to the
///    pre-created mount root, etc.). The trade-off: the sidecar shows
///    up in NTFS-level enumerations of the mountpoint. Acceptable
///    over failing the mount entirely; only triggers on restricted
///    parent ACLs.
///
/// Pre-fix this lived solely at `<root>/.heddle_projfs_id`, which had
/// two production-blocking problems:
///
/// 1. The file showed up in `dir`/`Get-ChildItem` listings of the
///    mountpoint. ProjFS treats files that exist *before*
///    `PrjMarkDirectoryAsPlaceholder` as "full files" and enumerates
///    them alongside placeholders, so users saw a `.heddle_projfs_id`
///    entry next to their projected source tree. Pure leakage of
///    internal metadata into the user-visible namespace.
/// 2. The kernel-side dir enumeration callback returned only the
///    PlatformShell entries, so listing the mounted root once via
///    NTFS (which sees the full file) and once via the projection
///    callback (which doesn't) produced an inconsistent picture
///    depending on which API the caller used to enumerate.
///
/// The r1 fix moved the sidecar unconditionally to the parent
/// directory, which fixed the listing leak but introduced a new
/// failure mode: mounts into a writable directory whose parent had
/// restricted ACLs would fail at sidecar-write time before
/// virtualization could even start. The two-tier model here keeps
/// the default behaviour clean while degrading gracefully when the
/// parent isn't writable.
///
/// ## Read order
///
/// Reads probe both locations: if a sidecar exists in either spot
/// (e.g. an older deployment wrote it to a fallback location and the
/// user later relaxed parent ACLs), we honour it before generating a
/// new GUID. The primary path wins if both exist.
fn load_or_create_instance_id(root: &Path) -> Result<GUID> {
    let primary = instance_id_sidecar_path_primary(root);
    let fallback = instance_id_sidecar_path_fallback(root);

    // Probe both locations on read. Primary wins if both exist —
    // we wrote there last, so it's the authoritative version.
    for candidate in [&primary, &fallback] {
        if let Ok(bytes) = std::fs::read(candidate)
            && bytes.len() == 16
        {
            return Ok(decode_guid(&bytes));
        }
    }

    let guid = GUID::new().map_err(|e| hresult_to_mount_error(e.code()))?;
    let bytes = encode_guid(&guid);

    // Best-effort: ensure the primary's parent exists. The CLI
    // usually creates `<repo_parent>/.<repo_name>-heddle-mounts/`
    // before calling us, so this is a no-op in the common path.
    if let Some(parent) = primary.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Try primary; on any I/O failure (commonly EACCES against a
    // restricted parent ACL, but we don't discriminate — any write
    // failure is the same signal: "can't put a sidecar here, try
    // the fallback") fall through to the in-root location.
    if std::fs::write(&primary, &bytes).is_ok() {
        return Ok(guid);
    }

    tracing::warn!(
        primary = %primary.display(),
        fallback = %fallback.display(),
        "projfs: parent-dir sidecar write failed; falling back to in-root sidecar. \
         The sidecar will appear in mountpoint listings; restrict only as needed.",
    );
    std::fs::write(&fallback, &bytes)
        .map_err(|e| MountError::Store(objects::error::HeddleError::Io(e)))?;
    Ok(guid)
}

/// Primary sidecar location: `<parent>/.<basename>.heddle-projfs-id`.
/// Returns the in-root path when `root` has no parent (the user
/// mounted a drive root, e.g. `C:\`) — there's no "outside" to put
/// the sidecar so the primary collapses to the fallback location.
fn instance_id_sidecar_path_primary(root: &Path) -> PathBuf {
    if let (Some(parent), Some(basename)) = (root.parent(), root.file_name()) {
        let mut name = OsString::from(".");
        name.push(basename);
        name.push(".heddle-projfs-id");
        parent.join(name)
    } else {
        instance_id_sidecar_path_fallback(root)
    }
}

/// Fallback sidecar location: always `<root>/.heddle-projfs-id`,
/// inside the virtualization root. Used when the primary location
/// can't be written; the trade-off is that this file shows up in
/// NTFS-level enumerations of the mountpoint.
fn instance_id_sidecar_path_fallback(root: &Path) -> PathBuf {
    root.join(".heddle-projfs-id")
}

/// Decode a 16-byte little-endian GUID buffer.
fn decode_guid(bytes: &[u8]) -> GUID {
    GUID::from_values(
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        u16::from_le_bytes([bytes[4], bytes[5]]),
        u16::from_le_bytes([bytes[6], bytes[7]]),
        [
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ],
    )
}

/// Encode a GUID into a 16-byte little-endian buffer (matches
/// `decode_guid` round-trip).
fn encode_guid(guid: &GUID) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(16);
    bytes.extend_from_slice(&guid.data1.to_le_bytes());
    bytes.extend_from_slice(&guid.data2.to_le_bytes());
    bytes.extend_from_slice(&guid.data3.to_le_bytes());
    bytes.extend_from_slice(&guid.data4);
    bytes
}

/// Build the single `PRJ_NOTIFICATION_MAPPING` we register with
/// `PrjStartVirtualizing`, covering the entire virtualization root.
///
/// The `NotificationRoot` field is required to be a non-null,
/// NUL-terminated wide string: an empty string (`L""`) means "the
/// whole virtualization root". `PrjStartVirtualizing` dereferences
/// this pointer synchronously to copy the relative path of the
/// directory the mapping applies to; passing a NULL `PCWSTR` here
/// crashed the host process with `STATUS_ACCESS_VIOLATION`
/// (heddle#108) before any callback could fire.
///
/// The empty string lives in `'static` storage so the pointer is
/// trivially valid for the duration of the `PrjStartVirtualizing`
/// call (and well beyond — the kernel copies the contents).
fn entire_root_notification_mapping(bits: PRJ_NOTIFY_TYPES) -> PRJ_NOTIFICATION_MAPPING {
    // A single NUL terminator — the wide-string form of `""`.
    static EMPTY_NOTIFICATION_ROOT: [u16; 1] = [0u16];
    PRJ_NOTIFICATION_MAPPING {
        NotificationBitMask: bits,
        NotificationRoot: PCWSTR(EMPTY_NOTIFICATION_ROOT.as_ptr()),
    }
}

fn hresult_to_mount_error(hr: HRESULT) -> MountError {
    let win32 = hr.0 as u32 & 0xFFFF;
    MountError::Store(objects::error::HeddleError::Io(
        std::io::Error::from_raw_os_error(win32 as i32),
    ))
}

fn mount_error_to_hresult(err: MountError) -> HRESULT {
    let errno = err.to_errno();
    // ERROR_FILE_NOT_FOUND-style mapping. `from_raw_os_error` on
    // Windows produces a Win32 error code, and ProjFS happily
    // accepts a wrapped Win32 HRESULT via `HRESULT_FROM_WIN32`.
    let win32 = errno_to_win32(errno);
    HRESULT(((win32 & 0xFFFF) | 0x8007_0000) as i32)
}

/// Approximate POSIX errno → Win32 error code translation. Only the
/// cases we actually surface from `MountError::to_errno` are mapped;
/// everything else degrades to ERROR_GEN_FAILURE.
///
/// `ESTALE` is POSIX-only and the Windows `libc` crate doesn't
/// export the constant. `MountError::to_errno` returns the POSIX
/// numeric value (`116`) verbatim on the Windows path, so the match
/// uses a literal here rather than `libc::ESTALE` for parity.
fn errno_to_win32(errno: i32) -> u32 {
    const ESTALE: i32 = 116; // POSIX ESTALE; libc on Windows doesn't define it.
    match errno {
        libc::ENOENT => 2,    // ERROR_FILE_NOT_FOUND
        libc::ENOTDIR => 267, // ERROR_DIRECTORY
        ESTALE => 1632,       // ERROR_FILE_INVALID (close enough)
        libc::EROFS => 19,    // ERROR_WRITE_PROTECT
        libc::EIO => 1117,    // ERROR_IO_DEVICE
        _ => 31,              // ERROR_GEN_FAILURE
    }
}

unsafe fn instance_from_context<'a>(
    context: *const std::ffi::c_void,
) -> Option<&'a InstanceContext> {
    if context.is_null() {
        return None;
    }
    // SAFETY: per `PrjStartVirtualizing`'s contract, ProjFS hands us
    // back the same `instance_context` pointer we registered. The
    // boxed `InstanceContext` lives until the matching
    // `PrjStopVirtualizing` returns.
    Some(unsafe { &*(context as *const InstanceContext) })
}

unsafe fn shell_from_context<'a>(
    context: *const std::ffi::c_void,
) -> Option<&'a Arc<dyn PlatformShell + Send + Sync>> {
    unsafe { instance_from_context(context) }.map(|ctx| &ctx.shell)
}

/// Catch any panic inside a ProjFS trampoline and convert it to an
/// HRESULT instead of letting the unwind cross the C ABI boundary.
/// Same rationale as `fskit::guarded_c_int`: Rust ≥1.81 converts
/// unwind-across-`extern "system"` to abort, so a panic in a
/// trampoline body would otherwise crash the whole host process and
/// take every projected mount with it. Each trampoline now logs the
/// panic and returns an EIO-class HRESULT for the one callback that
/// hit the bad path; the kernel surfaces it to the userland reader
/// as a normal I/O failure and the rest of the virtualization keeps
/// serving.
#[inline]
fn guarded_hresult<F: FnOnce() -> HRESULT>(label: &'static str, f: F) -> HRESULT {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(rc) => rc,
        Err(payload) => {
            let msg = panic_payload_str(&payload);
            tracing::error!(
                trampoline = label,
                %msg,
                "ProjFS trampoline panicked; returning HRESULT_FROM_WIN32(ERROR_IO_DEVICE)",
            );
            // ERROR_IO_DEVICE = 1117, the closest Win32 equivalent of EIO.
            mount_error_to_hresult(MountError::Store(objects::error::HeddleError::Io(
                std::io::Error::from_raw_os_error(1117),
            )))
        }
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

/// Read `data` field from a `PRJ_CALLBACK_DATA`, defensively.
unsafe fn callback_data<'a>(data: *const PRJ_CALLBACK_DATA) -> Option<&'a PRJ_CALLBACK_DATA> {
    if data.is_null() {
        None
    } else {
        // SAFETY: ProjFS owns the struct for the duration of the
        // callback.
        Some(unsafe { &*data })
    }
}

// ----------------------------------------------------------------
// Per-callback trampolines
// ----------------------------------------------------------------

unsafe extern "system" fn get_placeholder_info_trampoline(
    callback_data: *const PRJ_CALLBACK_DATA,
) -> HRESULT {
    guarded_hresult("get_placeholder_info", || unsafe {
        get_placeholder_info_impl(callback_data)
    })
}

unsafe fn get_placeholder_info_impl(callback_data: *const PRJ_CALLBACK_DATA) -> HRESULT {
    let Some(data) = (unsafe { callback_data_or_log("get_placeholder_info", callback_data) })
    else {
        return mount_error_to_hresult(MountError::Stale("null callback_data".into()));
    };
    let Some(shell) = (unsafe { shell_from_context(data.InstanceContext) }) else {
        return mount_error_to_hresult(MountError::Stale("null instance_context".into()));
    };

    let rel_path = unsafe { decode_wide(data.FilePathName) };
    let rel = Path::new(&rel_path);
    let node = match resolve_path(shell, rel) {
        Ok(n) => n,
        Err(e) => return mount_error_to_hresult(e),
    };
    let attrs = match shell.attrs(node) {
        Ok(a) => a,
        Err(e) => return mount_error_to_hresult(e),
    };

    let info = PRJ_PLACEHOLDER_INFO {
        FileBasicInfo: PRJ_FILE_BASIC_INFO {
            IsDirectory: matches!(attrs.kind, NodeKind::Directory).into(),
            FileSize: attrs.size as i64,
            CreationTime: Default::default(),
            LastAccessTime: Default::default(),
            LastWriteTime: Default::default(),
            ChangeTime: Default::default(),
            FileAttributes: if matches!(attrs.kind, NodeKind::Directory) {
                0x10 // FILE_ATTRIBUTE_DIRECTORY
            } else {
                0x80 // FILE_ATTRIBUTE_NORMAL
            },
        },
        ..PRJ_PLACEHOLDER_INFO::default()
    };

    // SAFETY: `info` is a stack value passed by reference; ProjFS
    // copies its contents synchronously.
    let rc = unsafe {
        PrjWritePlaceholderInfo(
            data.NamespaceVirtualizationContext,
            data.FilePathName,
            &info,
            std::mem::size_of::<PRJ_PLACEHOLDER_INFO>() as u32,
        )
    };
    match rc {
        Ok(()) => S_OK,
        Err(e) => e.code(),
    }
}

unsafe extern "system" fn get_file_data_trampoline(
    callback_data: *const PRJ_CALLBACK_DATA,
    byte_offset: u64,
    length: u32,
) -> HRESULT {
    guarded_hresult("get_file_data", || unsafe {
        get_file_data_impl(callback_data, byte_offset, length)
    })
}

unsafe fn get_file_data_impl(
    callback_data: *const PRJ_CALLBACK_DATA,
    byte_offset: u64,
    length: u32,
) -> HRESULT {
    let Some(data) = (unsafe { callback_data_or_log("get_file_data", callback_data) }) else {
        return mount_error_to_hresult(MountError::Stale("null callback_data".into()));
    };
    let Some(shell) = (unsafe { shell_from_context(data.InstanceContext) }) else {
        return mount_error_to_hresult(MountError::Stale("null instance_context".into()));
    };

    let rel_path = unsafe { decode_wide(data.FilePathName) };
    let node = match resolve_path(shell, Path::new(&rel_path)) {
        Ok(n) => n,
        Err(e) => return mount_error_to_hresult(e),
    };

    // ProjFS requires 64KiB-aligned (or sector-aligned) writes via
    // `PrjAllocateAlignedBuffer`. Allocate, fill, hand back.
    let buffer =
        unsafe { PrjAllocateAlignedBuffer(data.NamespaceVirtualizationContext, length as usize) };
    if buffer.is_null() {
        return mount_error_to_hresult(MountError::Store(objects::error::HeddleError::Io(
            std::io::Error::from_raw_os_error(libc::ENOMEM),
        )));
    }

    // SAFETY: buffer is `length` bytes, owned by us until we free it
    // or hand it to PrjWriteFileData. Slice lifetime is bounded by
    // this block.
    let slab = unsafe { std::slice::from_raw_parts_mut(buffer as *mut u8, length as usize) };
    let n = match shell.read(node, byte_offset, slab) {
        Ok(n) => n,
        Err(e) => {
            // SAFETY: buffer was produced by `PrjAllocateAlignedBuffer`
            // and is freed exactly once here on the error path.
            unsafe {
                PrjFreeAlignedBuffer(buffer);
            }
            return mount_error_to_hresult(e);
        }
    };

    let rc = unsafe {
        PrjWriteFileData(
            data.NamespaceVirtualizationContext,
            &data.DataStreamId,
            buffer,
            byte_offset,
            n as u32,
        )
    };
    // SAFETY: buffer is freed exactly once whether the write
    // succeeded or not.
    unsafe { PrjFreeAlignedBuffer(buffer) };
    match rc {
        Ok(()) => S_OK,
        Err(e) => e.code(),
    }
}

unsafe extern "system" fn start_dir_enum_trampoline(
    callback_data: *const PRJ_CALLBACK_DATA,
    enumeration_id: *const GUID,
) -> HRESULT {
    guarded_hresult("start_dir_enum", || unsafe {
        start_dir_enum_impl(callback_data, enumeration_id)
    })
}

unsafe fn start_dir_enum_impl(
    callback_data: *const PRJ_CALLBACK_DATA,
    enumeration_id: *const GUID,
) -> HRESULT {
    let Some(data) = (unsafe { callback_data_or_log("start_dir_enum", callback_data) }) else {
        return S_OK;
    };
    let Some(instance) = (unsafe { instance_from_context(data.InstanceContext) }) else {
        return S_OK;
    };
    if enumeration_id.is_null() {
        return S_OK;
    }
    let key = EnumKey::from_guid(unsafe { &*enumeration_id });
    // Insert a fresh empty state; the first `get_dir_enum` lazily
    // populates it from `shell.enumerate`. Doing the enumerate-walk
    // here would needlessly call into the shell when ProjFS only
    // wants to know whether to *allow* the open (which we always do).
    let mut guard = instance.enumerations.lock().expect("enumerations lock");
    guard.insert(key, EnumState::empty());
    S_OK
}

unsafe extern "system" fn end_dir_enum_trampoline(
    callback_data: *const PRJ_CALLBACK_DATA,
    enumeration_id: *const GUID,
) -> HRESULT {
    guarded_hresult("end_dir_enum", || unsafe {
        end_dir_enum_impl(callback_data, enumeration_id)
    })
}

unsafe fn end_dir_enum_impl(
    callback_data: *const PRJ_CALLBACK_DATA,
    enumeration_id: *const GUID,
) -> HRESULT {
    let Some(data) = (unsafe { callback_data_or_log("end_dir_enum", callback_data) }) else {
        return S_OK;
    };
    let Some(instance) = (unsafe { instance_from_context(data.InstanceContext) }) else {
        return S_OK;
    };
    if enumeration_id.is_null() {
        return S_OK;
    }
    let key = EnumKey::from_guid(unsafe { &*enumeration_id });
    let mut guard = instance.enumerations.lock().expect("enumerations lock");
    guard.remove(&key);
    S_OK
}

unsafe extern "system" fn get_dir_enum_trampoline(
    callback_data: *const PRJ_CALLBACK_DATA,
    enumeration_id: *const GUID,
    _search_expression: PCWSTR,
    dir_entry_buffer_handle: PRJ_DIR_ENTRY_BUFFER_HANDLE,
) -> HRESULT {
    guarded_hresult("get_dir_enum", || unsafe {
        get_dir_enum_impl(callback_data, enumeration_id, dir_entry_buffer_handle)
    })
}

unsafe fn get_dir_enum_impl(
    callback_data: *const PRJ_CALLBACK_DATA,
    enumeration_id: *const GUID,
    dir_entry_buffer_handle: PRJ_DIR_ENTRY_BUFFER_HANDLE,
) -> HRESULT {
    let Some(data) = (unsafe { callback_data_or_log("get_dir_enum", callback_data) }) else {
        return mount_error_to_hresult(MountError::Stale("null callback_data".into()));
    };
    let Some(instance) = (unsafe { instance_from_context(data.InstanceContext) }) else {
        return mount_error_to_hresult(MountError::Stale("null instance_context".into()));
    };
    let shell = &instance.shell;

    let rel_path = unsafe { decode_wide(data.FilePathName) };
    let dir_node = match resolve_path(shell, Path::new(&rel_path)) {
        Ok(n) => n,
        Err(e) => return mount_error_to_hresult(e),
    };

    if enumeration_id.is_null() {
        // Shouldn't happen — ProjFS always pairs `get` with a valid
        // `start`. Fail soft: emit one full pass and return. We can't
        // track a cursor without a key, so accept the duplicate-risk
        // on this pathological path rather than wedge the listing.
        let entries = match shell.enumerate(dir_node) {
            Ok(e) => e,
            Err(e) => return mount_error_to_hresult(e),
        };
        return match emit_entry_slice(&entries, dir_entry_buffer_handle) {
            Ok(_) => S_OK,
            Err(hr) => hr,
        };
    }

    let key = EnumKey::from_guid(unsafe { &*enumeration_id });
    let restart = (data.Flags.0 & PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN.0) != 0;

    let mut guard = instance.enumerations.lock().expect("enumerations lock");
    // Defensive: `get` without a prior `start` (unusual but observed
    // in some ProjFS versions on the first call after mount) — insert
    // an empty state so the populate path below runs.
    let state = guard.entry(key).or_insert_with(EnumState::empty);
    if restart {
        state.reset();
    }
    if !state.populated {
        match shell.enumerate(dir_node) {
            Ok(entries) => {
                state.entries = entries;
                state.cursor = 0;
                state.populated = true;
            }
            Err(e) => return mount_error_to_hresult(e),
        }
    }

    // Drain entries from the cursor forward, advancing on each
    // successfully-written entry. On `ERROR_INSUFFICIENT_BUFFER`,
    // leave `cursor` pointing at the entry that didn't fit so the
    // kernel's next call resumes there.
    match emit_entry_slice(&state.entries[state.cursor..], dir_entry_buffer_handle) {
        Ok(written) => {
            state.cursor += written;
            S_OK
        }
        Err(hr) => hr,
    }
}

/// Emit entries from a pre-fetched slice into the ProjFS buffer.
///
/// Returns:
///
/// * `Ok(n)` — `n` entries were successfully written (`n` may equal
///   the slice length, or be less when the kernel buffer ran out
///   AFTER at least one entry was written). The caller maps this to
///   `S_OK` and advances its resume cursor by `n`.
/// * `Err(HRESULT_FROM_WIN32(ERROR_INSUFFICIENT_BUFFER))` — the very
///   first entry didn't fit. ProjFS's contract requires this exact
///   HRESULT in that case so the kernel knows to retry the same
///   callback with a larger buffer. Returning `S_OK` (or `Ok(0)`
///   here) on a zero-progress invocation violates the protocol and,
///   in practice, can cause repeated retries without progress or
///   broken listings on deep trees with small initial buffers.
/// * `Err(other)` — any other failure from [`PrjFillDirEntryBuffer`]
///   propagates verbatim.
///
/// The "first entry vs. subsequent entry" distinction is the bit of
/// the contract that's easy to miss: it's per *callback invocation*,
/// not per *enumeration*. A second invocation of the callback (after
/// the kernel grew the buffer) starts a fresh "first entry" count
/// from the resumed cursor — the caller's `state.cursor` pointing at
/// a non-zero index doesn't change the rule that `i == 0` here means
/// "we wrote nothing in this call yet".
fn emit_entry_slice(
    entries: &[Entry],
    buffer: PRJ_DIR_ENTRY_BUFFER_HANDLE,
) -> std::result::Result<usize, HRESULT> {
    for (i, entry) in entries.iter().enumerate() {
        let basic = PRJ_FILE_BASIC_INFO {
            IsDirectory: matches!(entry.kind, NodeKind::Directory).into(),
            FileSize: entry.size as i64,
            CreationTime: Default::default(),
            LastAccessTime: Default::default(),
            LastWriteTime: Default::default(),
            ChangeTime: Default::default(),
            FileAttributes: if matches!(entry.kind, NodeKind::Directory) {
                0x10
            } else {
                0x80
            },
        };
        let mut name_wide = entry.name.encode_wide().collect::<Vec<u16>>();
        name_wide.push(0);
        // windows-rs 0.58 takes the basic-info pointer as
        // `Option<*const PRJ_FILE_BASIC_INFO>` (it was
        // `&mut PRJ_FILE_BASIC_INFO` in older bindings). `&basic`
        // produces a `&PRJ_FILE_BASIC_INFO` we coerce to a const
        // pointer for the kernel — the struct is read, not modified.
        let basic_ptr: *const PRJ_FILE_BASIC_INFO = &basic;
        let rc = unsafe {
            PrjFillDirEntryBuffer(PCWSTR(name_wide.as_ptr()), Some(basic_ptr), buffer)
        };
        if let Err(e) = rc {
            if e.code() == HRESULT::from(ERROR_INSUFFICIENT_BUFFER) {
                if i == 0 {
                    // Zero entries written this call → must surface
                    // INSUFFICIENT_BUFFER so the kernel grows the buffer
                    // and retries; an S_OK here would falsely tell the
                    // kernel we finished an empty page.
                    return Err(HRESULT::from(ERROR_INSUFFICIENT_BUFFER));
                }
                return Ok(i);
            }
            return Err(e.code());
        }
    }
    Ok(entries.len())
}

unsafe extern "system" fn notification_trampoline(
    callback_data: *const PRJ_CALLBACK_DATA,
    _is_directory: windows::Win32::Foundation::BOOLEAN,
    notification: PRJ_NOTIFICATION,
    destination_file_name: PCWSTR,
    _operation_parameters: *mut windows::Win32::Storage::ProjectedFileSystem::PRJ_NOTIFICATION_PARAMETERS,
) -> HRESULT {
    guarded_hresult("notification", || unsafe {
        notification_impl(callback_data, notification, destination_file_name)
    })
}

unsafe fn notification_impl(
    callback_data: *const PRJ_CALLBACK_DATA,
    notification: PRJ_NOTIFICATION,
    destination_file_name: PCWSTR,
) -> HRESULT {
    let Some(data) = (unsafe { callback_data_or_log("notification", callback_data) }) else {
        return S_OK;
    };
    let Some(instance) = (unsafe { instance_from_context(data.InstanceContext) }) else {
        return S_OK;
    };
    let shell = &instance.shell;
    let virt_root = instance.virtualization_root.as_path();

    let rel_path = unsafe { decode_wide(data.FilePathName) };
    let rel = Path::new(&rel_path);
    let node = match resolve_path(shell, rel) {
        Ok(n) => n,
        Err(_) => {
            // A file that's been deleted (or never existed) is fine
            // for notifications — just bail.
            return S_OK;
        }
    };

    match notification {
        n if n == PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_MODIFIED => {
            // ProjFS does not deliver per-write callbacks. Read the
            // hydrated file back from NTFS — at the path we registered
            // in `PrjStartVirtualizing`, not at `std::env::current_dir`
            // which silently drops the write if the host process
            // chdir'd elsewhere — and synthesize a single write+flush
            // so the existing CAS promotion path runs.
            let full_path = virt_root.join(rel);
            match std::fs::read(&full_path) {
                Ok(contents) => {
                    if let Err(e) = shell.write(node, 0, &contents) {
                        warn!(
                            path = %full_path.display(),
                            error = ?e,
                            "projfs: shell.write after close-modified failed",
                        );
                        return mount_error_to_hresult(e);
                    }
                    if let Err(e) = shell.flush(node) {
                        warn!(
                            path = %full_path.display(),
                            error = ?e,
                            "projfs: shell.flush after close-modified failed",
                        );
                        return mount_error_to_hresult(e);
                    }
                }
                Err(e) => {
                    warn!(
                        path = %full_path.display(),
                        error = ?e,
                        "projfs: could not re-read hydrated file on close-modified",
                    );
                    return mount_error_to_hresult(MountError::Store(
                        objects::error::HeddleError::Io(e),
                    ));
                }
            }
        }
        n if n == PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_DELETED => {
            // The core records deletions in the pending tier on
            // `release` of a deleted inode; surface this notification
            // as a release so the same path runs.
            if let Err(e) = shell.release(node) {
                warn!(
                    path = %rel.display(),
                    error = ?e,
                    "projfs: shell.release after close-deleted failed",
                );
            }
        }
        n if n == PRJ_NOTIFICATION_FILE_RENAMED => {
            // Rename = delete-old + write-new. We surface both legs
            // and propagate errors via HRESULT — previously the
            // `let _ = shell.write/flush(...)` swallow lost writes
            // silently the same way the data_root bug did.
            if let Err(e) = shell.release(node) {
                warn!(
                    path = %rel.display(),
                    error = ?e,
                    "projfs: rename old-path release failed",
                );
                return mount_error_to_hresult(e);
            }
            let dest = unsafe { decode_wide(destination_file_name) };
            if !dest.is_empty() {
                let dest_path = Path::new(&dest);
                match resolve_path(shell, dest_path) {
                    Ok(new_node) => {
                        let full_path = virt_root.join(dest_path);
                        match std::fs::read(&full_path) {
                            Ok(contents) => {
                                if let Err(e) = shell.write(new_node, 0, &contents) {
                                    warn!(
                                        path = %full_path.display(),
                                        error = ?e,
                                        "projfs: rename new-path write failed",
                                    );
                                    return mount_error_to_hresult(e);
                                }
                                if let Err(e) = shell.flush(new_node) {
                                    warn!(
                                        path = %full_path.display(),
                                        error = ?e,
                                        "projfs: rename new-path flush failed",
                                    );
                                    return mount_error_to_hresult(e);
                                }
                            }
                            Err(e) => {
                                warn!(
                                    path = %full_path.display(),
                                    error = ?e,
                                    "projfs: could not re-read hydrated file on rename",
                                );
                                return mount_error_to_hresult(MountError::Store(
                                    objects::error::HeddleError::Io(e),
                                ));
                            }
                        }
                    }
                    Err(e) => {
                        warn!(
                            dest = %dest_path.display(),
                            error = ?e,
                            "projfs: rename new-path resolve failed",
                        );
                        return mount_error_to_hresult(e);
                    }
                }
            }
        }
        _ => {}
    }
    S_OK
}

/// Wrap [`callback_data`] with a single log site so we don't sprinkle
/// `null callback_data` warnings across every trampoline.
unsafe fn callback_data_or_log<'a>(
    site: &str,
    data: *const PRJ_CALLBACK_DATA,
) -> Option<&'a PRJ_CALLBACK_DATA> {
    let opt = unsafe { callback_data(data) };
    if opt.is_none() {
        warn!(site, "projfs: null PRJ_CALLBACK_DATA");
    }
    opt
}

// ----------------------------------------------------------------
// Tests
// ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::tests::mocks::CountingShell;

    /// Construct a ProjFsShell, drop it, and verify the inner shell
    /// is reclaimed exactly once. We don't actually start
    /// virtualization here — just the box ownership transfer.
    ///
    /// Marked `#[ignore]` because the `windows` crate dep is only
    /// linked when the `projfs` feature is on; running this without
    /// `--features projfs` will produce a link error.
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "requires the projfs feature; opt-in via --features projfs"]
    fn projfs_shell_does_not_leak_when_mount_skipped() {
        let (counting, drops) = CountingShell::new();
        let shell = Arc::new(counting);
        let projfs = ProjFsShell::from_shell(shell);
        // Construction alone shouldn't drop the inner shell.
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(projfs);
        // Dropping the shell without ever mounting reclaims the Arc
        // immediately (no kernel handoff happened, so no leak).
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "requires the projfs feature; opt-in via --features projfs"]
    fn is_runtime_available_does_not_panic() {
        let _ = ProjFsShell::is_runtime_available();
    }

    /// The primary instance-ID sidecar must land in the *parent*
    /// directory, not inside the virtualization root. Pre-fix the
    /// file lived at `<root>/.heddle_projfs_id` and leaked into
    /// `dir`/`ls` listings; the regression test in
    /// `tests/projfs_smoke.rs` covers the on-disk side, this unit
    /// test covers the pure path-derivation function in isolation so
    /// a refactor that silently moves the sidecar back inside the
    /// root trips here before it reaches the smoke matrix.
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "requires the projfs feature; opt-in via --features projfs"]
    fn primary_instance_id_sidecar_lives_outside_the_virtualization_root() {
        let root = Path::new("C:\\users\\test\\.heddle-mounts\\thread-x");
        let sidecar = instance_id_sidecar_path_primary(root);
        assert!(
            !sidecar.starts_with(root),
            "primary sidecar must be outside virt root, got {}",
            sidecar.display(),
        );
        // Sanity: the file name encodes the basename so multiple
        // sibling mounts get distinct sidecars.
        assert!(
            sidecar.file_name().unwrap().to_string_lossy().contains("thread-x"),
            "sidecar name must include the mount basename, got {}",
            sidecar.display(),
        );
    }

    /// The fallback sidecar — used when the primary location can't
    /// be written (restricted parent ACL, service-account scenario)
    /// — lives inside the virtualization root. Documenting it here
    /// to lock in the contract for callers that need to clean up a
    /// mount: removing both `<parent>/.<basename>.heddle-projfs-id`
    /// AND `<root>/.heddle-projfs-id` is required to reset the
    /// instance identity.
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "requires the projfs feature; opt-in via --features projfs"]
    fn fallback_instance_id_sidecar_lives_inside_the_virtualization_root() {
        let root = Path::new("C:\\users\\test\\.heddle-mounts\\thread-x");
        let sidecar = instance_id_sidecar_path_fallback(root);
        assert!(
            sidecar.starts_with(root),
            "fallback sidecar must be inside virt root, got {}",
            sidecar.display(),
        );
        assert_eq!(sidecar.file_name().unwrap(), ".heddle-projfs-id");
    }

    /// Regression test for heddle#108: STATUS_ACCESS_VIOLATION in
    /// `PrjStartVirtualizing`.
    ///
    /// Pre-fix, `mount_background` built its single
    /// `PRJ_NOTIFICATION_MAPPING` with `NotificationRoot:
    /// PCWSTR::null()`. The ProjFS kernel dereferences that pointer
    /// to copy the relative path of the directory the mapping
    /// applies to (an empty string means "the whole virtualization
    /// root"), so a null caused the host process to crash with
    /// STATUS_ACCESS_VIOLATION before any callback fired — every
    /// projfs-smoke test failed at the `mount_background` step.
    ///
    /// The fix points `NotificationRoot` at a NUL-terminated empty
    /// wide string. This test pins the contract: the mapping
    /// `mount_background` ships into the kernel must (a) have a
    /// non-null `NotificationRoot` and (b) that pointer must point
    /// at a NUL terminator (= empty string).
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "requires the projfs feature; opt-in via --features projfs"]
    fn notification_mapping_root_is_non_null_empty_wide_string_heddle108() {
        let mapping = entire_root_notification_mapping(PRJ_NOTIFY_TYPES(0));
        assert!(
            !mapping.NotificationRoot.0.is_null(),
            "NotificationRoot must not be null — PrjStartVirtualizing \
             STATUS_ACCESS_VIOLATIONs on null (heddle#108)",
        );
        // SAFETY: the fix points NotificationRoot at a 'static
        // [u16; 1] = [0]; dereferencing one u16 is in-bounds.
        let first = unsafe { *mapping.NotificationRoot.0 };
        assert_eq!(
            first, 0u16,
            "NotificationRoot must point at a NUL terminator (= empty \
             string, meaning 'the whole virtualization root')",
        );
    }

    /// `EnumKey` is the hash-able wrapper around `GUID` we use to
    /// key the per-enumeration cursor map. Two `EnumKey`s built
    /// from the same GUID must compare equal and hash to the same
    /// bucket — otherwise the cursor cache would miss every
    /// directory enumeration and we'd re-emit the head of the
    /// listing on every kernel callback.
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "requires the projfs feature; opt-in via --features projfs"]
    fn enum_key_round_trips_through_guid() {
        let g = GUID::from_values(0x1234_5678, 0x9abc, 0xdef0, [1, 2, 3, 4, 5, 6, 7, 8]);
        let a = EnumKey::from_guid(&g);
        let b = EnumKey::from_guid(&g);
        assert_eq!(a, b, "same GUID must produce equal EnumKey");

        // Two distinct GUIDs must produce distinct keys (the only
        // way that fails is a collision in the byte canonicalisation).
        let g2 = GUID::from_values(0x1234_5678, 0x9abc, 0xdef0, [1, 2, 3, 4, 5, 6, 7, 9]);
        assert_ne!(
            a,
            EnumKey::from_guid(&g2),
            "different GUIDs must produce different EnumKeys",
        );
    }
}
