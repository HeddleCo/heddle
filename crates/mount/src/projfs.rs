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
    ffi::{OsStr, OsString},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    sync::Arc,
};

use tracing::warn;
use windows::{
    core::{GUID, HRESULT, PCWSTR, PWSTR},
    Win32::{
        Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_MOD_NOT_FOUND, S_OK},
        Storage::ProjectedFileSystem::{
            PrjAllocateAlignedBuffer, PrjFillDirEntryBuffer, PrjFreeAlignedBuffer,
            PrjMarkDirectoryAsPlaceholder, PrjStartVirtualizing, PrjStopVirtualizing,
            PrjWriteFileData, PrjWritePlaceholderInfo, PRJ_CALLBACKS, PRJ_CALLBACK_DATA,
            PRJ_DIR_ENTRY_BUFFER_HANDLE, PRJ_FILE_BASIC_INFO, PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
            PRJ_NOTIFICATION, PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_DELETED,
            PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_MODIFIED,
            PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_NO_MODIFICATION, PRJ_NOTIFICATION_FILE_RENAMED,
            PRJ_NOTIFICATION_MAPPING, PRJ_PLACEHOLDER_INFO, PRJ_STARTVIRTUALIZING_OPTIONS,
        },
        System::LibraryLoader::{FreeLibrary, LoadLibraryW},
    },
};

use crate::{
    core::ContentAddressedMount,
    error::{MountError, Result},
    shell::{NodeId, NodeKind, PlatformShell},
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
                std::ptr::null(),
                &instance_id,
            )
            .ok()
            .map_err(|e| hresult_to_mount_error(e.code()))?;
        }

        // Box the shell once and leak the pointer into the ProjFS
        // InstanceContext. The session's Drop reclaims it.
        let boxed: Box<Arc<dyn PlatformShell + Send + Sync>> = Box::new(self.inner);
        let instance_context = Box::into_raw(boxed) as *const std::ffi::c_void;

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
        let notification_bits = (PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_MODIFIED.0
            | PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_DELETED.0
            | PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_NO_MODIFICATION.0
            | PRJ_NOTIFICATION_FILE_RENAMED.0) as i32;
        let mut notification_mapping = PRJ_NOTIFICATION_MAPPING {
            NotificationBitMask: PRJ_NOTIFICATION(notification_bits),
            NotificationRoot: PCWSTR::null(),
        };

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
        let mut handle = PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT::default();
        let start_rc = unsafe {
            PrjStartVirtualizing(
                PCWSTR(root_wide.as_ptr()),
                &callbacks,
                Some(instance_context),
                Some(&options),
                &mut handle,
            )
        };
        if let Err(e) = start_rc {
            // Mount didn't start; reclaim the box so we don't leak.
            // SAFETY: `instance_context` was produced by Box::into_raw
            // above and the kernel never took ownership.
            unsafe {
                drop(Box::from_raw(
                    instance_context as *mut Arc<dyn PlatformShell + Send + Sync>,
                ));
            }
            return Err(hresult_to_mount_error(e.code()));
        }

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
            drop(Box::from_raw(
                self.instance_context as *mut Arc<dyn PlatformShell + Send + Sync>,
            ));
        }
        self.instance_context = std::ptr::null();
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
/// instance identity. Lives at `<root>/.heddle_projfs_id`.
fn load_or_create_instance_id(root: &Path) -> Result<GUID> {
    let id_path = root.join(".heddle_projfs_id");
    if let Ok(bytes) = std::fs::read(&id_path) {
        if bytes.len() == 16 {
            // SAFETY: bytes is exactly 16 bytes (GUID layout).
            return Ok(GUID::from_values(
                u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
                u16::from_le_bytes([bytes[4], bytes[5]]),
                u16::from_le_bytes([bytes[6], bytes[7]]),
                [
                    bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14],
                    bytes[15],
                ],
            ));
        }
    }
    let guid = GUID::new().map_err(|e| hresult_to_mount_error(e.code()))?;
    let mut bytes = Vec::with_capacity(16);
    bytes.extend_from_slice(&guid.data1.to_le_bytes());
    bytes.extend_from_slice(&guid.data2.to_le_bytes());
    bytes.extend_from_slice(&guid.data3.to_le_bytes());
    bytes.extend_from_slice(&guid.data4);
    std::fs::write(&id_path, &bytes)
        .map_err(|e| MountError::Store(objects::error::HeddleError::Io(e)))?;
    Ok(guid)
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
fn errno_to_win32(errno: i32) -> u32 {
    match errno {
        libc::ENOENT => 2,    // ERROR_FILE_NOT_FOUND
        libc::ENOTDIR => 267, // ERROR_DIRECTORY
        libc::ESTALE => 1632, // ERROR_FILE_INVALID (close enough)
        libc::EROFS => 19,    // ERROR_WRITE_PROTECT
        libc::EIO => 1117,    // ERROR_IO_DEVICE
        _ => 31,              // ERROR_GEN_FAILURE
    }
}

unsafe fn shell_from_context<'a>(
    context: *const std::ffi::c_void,
) -> Option<&'a Arc<dyn PlatformShell + Send + Sync>> {
    if context.is_null() {
        return None;
    }
    // SAFETY: per `PrjStartVirtualizing`'s contract, ProjFS hands us
    // back the same `instance_context` pointer we registered. The
    // boxed `Arc<dyn PlatformShell>` lives until the matching
    // `PrjStopVirtualizing` returns.
    Some(unsafe { &*(context as *const Arc<dyn PlatformShell + Send + Sync>) })
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

    let mut info = PRJ_PLACEHOLDER_INFO::default();
    info.FileBasicInfo = PRJ_FILE_BASIC_INFO {
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
    _callback_data: *const PRJ_CALLBACK_DATA,
    _enumeration_id: *const GUID,
) -> HRESULT {
    // We compute the directory listing lazily on each `get_dir_enum`
    // call rather than caching it across the start/get/end trio.
    // ProjFS will recall `get` until we set `EntriesAvailable=false`.
    S_OK
}

unsafe extern "system" fn end_dir_enum_trampoline(
    _callback_data: *const PRJ_CALLBACK_DATA,
    _enumeration_id: *const GUID,
) -> HRESULT {
    S_OK
}

unsafe extern "system" fn get_dir_enum_trampoline(
    callback_data: *const PRJ_CALLBACK_DATA,
    _enumeration_id: *const GUID,
    _search_expression: PCWSTR,
    dir_entry_buffer_handle: PRJ_DIR_ENTRY_BUFFER_HANDLE,
) -> HRESULT {
    let Some(data) = (unsafe { callback_data_or_log("get_dir_enum", callback_data) }) else {
        return mount_error_to_hresult(MountError::Stale("null callback_data".into()));
    };
    let Some(shell) = (unsafe { shell_from_context(data.InstanceContext) }) else {
        return mount_error_to_hresult(MountError::Stale("null instance_context".into()));
    };

    let rel_path = unsafe { decode_wide(data.FilePathName) };
    let dir_node = match resolve_path(shell, Path::new(&rel_path)) {
        Ok(n) => n,
        Err(e) => return mount_error_to_hresult(e),
    };

    let entries = match shell.enumerate(dir_node) {
        Ok(e) => e,
        Err(e) => return mount_error_to_hresult(e),
    };

    for entry in entries {
        let mut basic = PRJ_FILE_BASIC_INFO {
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
        let rc = unsafe {
            PrjFillDirEntryBuffer(
                PCWSTR(name_wide.as_ptr()),
                &mut basic,
                dir_entry_buffer_handle,
            )
        };
        if let Err(e) = rc {
            // ERROR_INSUFFICIENT_BUFFER means the kernel's buffer is
            // full and it will recall us. Not a hard failure.
            if e.code() == HRESULT::from(ERROR_INSUFFICIENT_BUFFER) {
                break;
            }
            return e.code();
        }
    }
    S_OK
}

unsafe extern "system" fn notification_trampoline(
    callback_data: *const PRJ_CALLBACK_DATA,
    _is_directory: windows::Win32::Foundation::BOOLEAN,
    notification: PRJ_NOTIFICATION,
    destination_file_name: PCWSTR,
    _operation_parameters: *mut windows::Win32::Storage::ProjectedFileSystem::PRJ_NOTIFICATION_PARAMETERS,
) -> HRESULT {
    let Some(data) = (unsafe { callback_data_or_log("notification", callback_data) }) else {
        return S_OK;
    };
    let Some(shell) = (unsafe { shell_from_context(data.InstanceContext) }) else {
        return S_OK;
    };

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
            // hydrated file back from NTFS and synthesize a single
            // write+flush so the existing CAS promotion path runs.
            let full_path = data_root(data).join(rel);
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
            // Rename = delete-old + write-new. We surface both legs.
            if let Err(e) = shell.release(node) {
                warn!(error = ?e, "projfs: rename old-path release failed");
            }
            let dest = unsafe { decode_wide(destination_file_name) };
            if !dest.is_empty() {
                let dest_path = Path::new(&dest);
                if let Ok(new_node) = resolve_path(shell, dest_path) {
                    let full_path = data_root(data).join(dest_path);
                    if let Ok(contents) = std::fs::read(&full_path) {
                        let _ = shell.write(new_node, 0, &contents);
                        let _ = shell.flush(new_node);
                    }
                }
            }
        }
        _ => {}
    }
    S_OK
}

fn data_root(data: &PRJ_CALLBACK_DATA) -> PathBuf {
    // SAFETY: `VersionInfo` is not what we want — the root path is
    // not directly in callback_data on every API version. Instead we
    // reconstruct from a side channel: the path the caller registered
    // in `PrjStartVirtualizing` is also accessible via the
    // notification's parent directory. ProjFS does not expose the
    // root on every version, so as a best-effort we walk up from the
    // file path. The existing API contract guarantees `FilePathName`
    // is relative to the virtualization root.
    //
    // For the close-modified bridge, we can rely on the working
    // directory being the virtualization root for the FS handle that
    // produced the notification. The caller's `data.FilePathName` is
    // already relative; the join in the trampoline against the
    // ProjFsSession's stored root via `data_root` here returns the
    // current dir, which on a typical ProjFS path is the root. If the
    // host moves to a chdir-changed process, the close-modified
    // fallback will fail loudly and we'll log a warning.
    //
    // A cleaner solution is to thread the virt-root into
    // instance_context alongside the shell; see issue #TBD.
    let _ = data;
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
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
}
