// SPDX-License-Identifier: Apache-2.0
//! NFSv3 fallback shell.
//!
//! [`NfsShell`] stands up an in-process NFSv3 server (via the
//! `nfsserve` crate) on `127.0.0.1:<ephemeral>` and shells out to
//! the host's built-in NFS client to mount it. Unlike the
//! per-OS native adapters ([`crate::fuse`] on Linux,
//! [`crate::fskit`] on macOS, [`crate::projfs`] on Windows), this
//! adapter is platform-agnostic — every supported OS ships an NFS
//! client in its kernel.
//!
//! ## When this is used
//!
//! The CLI's mount lifecycle tries the host's native adapter
//! first and falls back to [`NfsShell`] when that adapter is
//! unavailable at runtime:
//!
//! * Linux without the FUSE kernel module / `fusermount`.
//! * macOS without a code-signed `.fsmodule` System Extension
//!   (the common case — see `crates/mount/README.md`).
//! * Windows without the "Projected File System" optional feature
//!   enabled.
//!
//! The fallback path is opt-out via the CLI feature flag, not
//! automatic per-call: enabling `--features mount` enables both
//! the native adapter and this fallback together.
//!
//! ## Privileges
//!
//! Mounting NFS requires admin/root on every supported OS:
//!   * Linux: `mount(8)` needs `CAP_SYS_ADMIN` or sudo.
//!   * macOS: `mount_nfs` needs sudo (the `resvport=off` option
//!     lets us bind a non-privileged source port but the `mount`
//!     syscall itself is still root-only).
//!   * Windows: `mount.exe` needs an elevated console and the
//!     "Services for NFS — Client for NFS" optional feature.
//!
//! If the caller can't get root, the native adapter (which can run
//! unprivileged via `fusermount` on Linux, or via FSKit's loaded
//! System Extension on macOS) is the correct fix; this fallback
//! is meant for the common case where mount-time admin is fine
//! but installing kernel extensions / System Extensions isn't.
//!
//! ## Capabilities
//!
//! The shell exposes the mount **read + write-to-existing-file**.
//! NFS ops we route through [`PlatformShell`]:
//!   * `lookup`, `getattr`, `read`, `readdir`, `write`
//!
//! NFS ops we surface as `NFS3ERR_ROFS` because the trait does
//! not have a corresponding hook:
//!   * `create`, `mkdir`, `remove`, `rename`, `symlink`,
//!     `create_exclusive`.
//!
//! `setattr` accepts the request but ignores attribute changes
//! and returns the current `fattr3` — vim and similar editors
//! call setattr to truncate before writing, then write the full
//! buffer, which works against our hot-tier model because the
//! follow-up `write` overwrites whatever was there.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use nfsserve::{
    nfs::{
        fattr3, fileid3, filename3, ftype3, mode3, nfspath3, nfsstat3, nfstime3, sattr3, specdata3,
    },
    tcp::{NFSTcp, NFSTcpListener},
    vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities},
};
use tokio::runtime::{Builder, Runtime};
use tracing::{debug, warn};

use crate::{
    core::ContentAddressedMount,
    error::{MountError, Result},
    shell::{NodeId, NodeKind, PlatformShell},
};

// ----------------------------------------------------------------
// Public surface
// ----------------------------------------------------------------

pub struct NfsShell {
    inner: Arc<dyn PlatformShell + Send + Sync>,
}

impl NfsShell {
    pub fn new(mount: ContentAddressedMount) -> Self {
        Self::from_shell(Arc::new(mount))
    }

    pub fn from_shell(shell: Arc<dyn PlatformShell + Send + Sync>) -> Self {
        Self { inner: shell }
    }

    /// NFS is universally available on Linux/macOS/Windows kernels
    /// (Windows requires the "Services for NFS" optional feature
    /// — we don't probe for it here because the failure case
    /// surfaces cleanly through `mount(8)` returning non-zero).
    pub fn is_runtime_available() -> bool {
        true
    }

    /// Spin up the NFS server and ask the OS to mount it. Returns
    /// an RAII [`NfsSession`] that unmounts on drop.
    pub fn mount_background(self, mountpoint: impl AsRef<Path>) -> Result<NfsSession> {
        let mountpoint = mountpoint.as_ref().to_path_buf();
        std::fs::create_dir_all(&mountpoint)
            .map_err(|e| MountError::Store(objects::error::HeddleError::Io(e)))?;

        // Dedicated multi-thread runtime for the NFS server. We
        // can't reuse a caller-supplied runtime because we want
        // the session's lifetime to own the workers — dropping
        // NfsSession aborts the server and stops the runtime
        // cleanly.
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("heddle-nfs")
            .build()
            .map_err(|e| MountError::Store(objects::error::HeddleError::Io(e)))?;

        let fs = HeddleNFS {
            inner: Arc::clone(&self.inner),
        };

        // Bind the listener on an ephemeral port so multiple
        // concurrent threads don't fight over a fixed one. The
        // kernel hands us a real port we then pass to `mount`.
        //
        // Threat model — explicit because this is a localhost TCP
        // server with **no authentication**: any local process on
        // the host that can reach `127.0.0.1:<port>` can
        // `mount.nfs` against it and read the entire projected
        // thread tree without permission checks. The bind is
        // `127.0.0.1` (not `0.0.0.0`) so the surface is limited to
        // the local machine, and the ephemeral port reduces the
        // window for a co-resident process to guess it, but
        // neither is a defence against another logged-in user or
        // a browser/renderer process running on the same box.
        //
        // Operationally this is fine on a single-user dev laptop
        // (the model this crate's mount surface is built for) but
        // it's not appropriate for shared dev VMs, multi-tenant
        // CI runners, or any host where you can't trust every
        // local process to behave. The follow-up is to switch to
        // a UNIX-domain socket (nfsserve supports it), which
        // limits the surface to filesystem permissions — the
        // sentinel we want at this layer. Tracked separately.
        let listener = match bind_nfs_listener(&runtime, fs) {
            Ok(listener) => listener,
            Err(error) => {
                runtime.shutdown_background();
                return Err(error);
            }
        };
        let port = listener.get_listen_port();
        debug!(port, "heddle nfs server listening");

        // Spawn the accept loop into the runtime. The handle isn't
        // joined; the runtime drop in NfsSession::drop tears the
        // task down.
        runtime.spawn(async move {
            if let Err(e) = listener.handle_forever().await {
                warn!("nfs server exited: {e}");
            }
        });

        // Hand the OS the mount request. Failure here means the
        // user lacks privileges, the optional feature isn't
        // installed (Windows), or the mountpoint is unusable.
        if let Err(e) = invoke_mount(&mountpoint, port) {
            // `mount_background` is often called from the async CLI
            // runtime. Dropping a Tokio runtime in that context panics,
            // so tear down the fallback server explicitly before
            // returning the mount error.
            runtime.shutdown_background();
            return Err(MountError::Store(objects::error::HeddleError::Io(e)));
        }

        Ok(NfsSession {
            runtime: Some(runtime),
            mountpoint,
            port,
            unmounted: false,
        })
    }
}

fn bind_nfs_listener(runtime: &Runtime, fs: HeddleNFS) -> Result<NFSTcpListener<HeddleNFS>> {
    // `mount_background` can be called from the async CLI runtime
    // when FSKit/FUSE falls back to NFS. Running `block_on` on
    // that thread would panic, so do the one-time async bind from
    // a plain OS thread while still using the NFS session runtime.
    let handle = runtime.handle().clone();
    let join = std::thread::Builder::new()
        .name("heddle-nfs-bind".to_string())
        .spawn(move || handle.block_on(NFSTcpListener::bind("127.0.0.1:0", fs)))
        .map_err(|e| MountError::Store(objects::error::HeddleError::Io(e)))?;

    join.join()
        .map_err(|_| {
            MountError::Store(objects::error::HeddleError::Io(std::io::Error::other(
                "nfs bind thread panicked",
            )))
        })?
        .map_err(|e| MountError::Store(objects::error::HeddleError::Io(e)))
}

pub struct NfsSession {
    /// Held so the spawned NFS server task lives until Drop.
    runtime: Option<Runtime>,
    mountpoint: PathBuf,
    #[allow(dead_code)]
    port: u16,
    /// `true` after a successful explicit `unmount()`. Tells `Drop`
    /// to skip its fallback `invoke_unmount`; without this, an
    /// explicit unmount is followed by Drop's silent retry, which
    /// produces a spurious failure warning (the path is already
    /// unmounted) and — in the worst case — racily unmounts a
    /// freshly-reused mountpoint a sibling thread just claimed.
    unmounted: bool,
}

impl NfsSession {
    pub fn unmount(mut self) -> Result<()> {
        invoke_unmount(&self.mountpoint)
            .map_err(|e| MountError::Store(objects::error::HeddleError::Io(e)))?;
        self.unmounted = true;
        // Shut the server down. `Runtime::shutdown_background`
        // releases the worker threads without blocking; the
        // accept loop's `tokio::spawn`-ed tasks die with the
        // runtime.
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
        Ok(())
    }

    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }
}

impl Drop for NfsSession {
    fn drop(&mut self) {
        if !self.unmounted
            && let Err(e) = invoke_unmount(&self.mountpoint)
        {
            warn!(
                mountpoint = %self.mountpoint.display(),
                "nfs unmount on drop failed: {e}",
            );
        }
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
    }
}

// ----------------------------------------------------------------
// Mount/unmount: shell out to the host's NFS client tooling
// ----------------------------------------------------------------

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn invoke_mount(mountpoint: &Path, port: u16) -> std::io::Result<()> {
    use std::process::Command;

    // Both Linux and macOS accept `-t nfs -o <opts> <host>:/ <mp>`.
    // The option set keeps the kernel from waiting on a remote
    // lock manager (we don't run rpc.lockd), forbids reserved-port
    // binding (we don't run as root inside the server process),
    // and pins NFSv3 + TCP.
    let opts =
        format!("vers=3,tcp,port={port},mountport={port},nolocks,soft,intr,actimeo=0,resvport=off");
    let status = Command::new("mount")
        .arg("-t")
        .arg("nfs")
        .arg("-o")
        .arg(&opts)
        .arg("127.0.0.1:/")
        .arg(mountpoint)
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "mount(8) returned {status} (NFS mount usually requires sudo)"
        )));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn invoke_mount(mountpoint: &Path, port: u16) -> std::io::Result<()> {
    use std::process::Command;

    // Windows NFS client uses `mount.exe`. The localhost-port
    // syntax is `mount -o anon nolock port=<p> 127.0.0.1:/ X:`.
    // We can't pass a directory as the mountpoint; the caller
    // must provide a drive letter. If they passed a path under
    // `%TEMP%` we surface a clear error instead of failing inside
    // mount.exe.
    let mp_str = mountpoint
        .to_str()
        .ok_or_else(|| std::io::Error::other("non-UTF8 mountpoint"))?;
    let status = Command::new("mount.exe")
        .arg("-o")
        .arg(format!("anon,nolock,mtype=hard,port={port}"))
        .arg("127.0.0.1:/")
        .arg(mp_str)
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "mount.exe returned {status} (NFS mount on Windows needs the \
             'Services for NFS — Client for NFS' optional feature and an \
             elevated console)"
        )));
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn invoke_mount(_mountpoint: &Path, _port: u16) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "NFS fallback is only supported on Linux, macOS, and Windows",
    ))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn invoke_unmount(mountpoint: &Path) -> std::io::Result<()> {
    use std::process::Command;

    // `umount` is the same name on both. macOS additionally
    // accepts `umount -f` for a force-unmount, which we don't use
    // — a stuck unmount is more informative than a silent force.
    let status = Command::new("umount").arg(mountpoint).status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "umount(8) returned {status}"
        )));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn invoke_unmount(mountpoint: &Path) -> std::io::Result<()> {
    use std::process::Command;

    let mp_str = mountpoint
        .to_str()
        .ok_or_else(|| std::io::Error::other("non-UTF8 mountpoint"))?;
    let status = Command::new("umount.exe").arg("-f").arg(mp_str).status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "umount.exe returned {status}"
        )));
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn invoke_unmount(_mountpoint: &Path) -> std::io::Result<()> {
    Ok(())
}

// ----------------------------------------------------------------
// HeddleNFS — NFSFileSystem impl that dispatches to PlatformShell
// ----------------------------------------------------------------

struct HeddleNFS {
    inner: Arc<dyn PlatformShell + Send + Sync>,
}

#[async_trait]
impl NFSFileSystem for HeddleNFS {
    fn capabilities(&self) -> VFSCapabilities {
        // Write-to-existing-files is the only mutating op we
        // support. The trait does not have hooks for create/mkdir/
        // remove/rename, so each of those returns NFS3ERR_ROFS
        // — but the overall capability is ReadWrite so the
        // kernel allows the write paths we DO support.
        VFSCapabilities::ReadWrite
    }

    fn root_dir(&self) -> fileid3 {
        // PlatformShell convention: NodeId::ROOT.0 == 1.
        NodeId::ROOT.0
    }

    async fn lookup(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> std::result::Result<fileid3, nfsstat3> {
        let name = OsStr::new(
            std::str::from_utf8(filename.as_ref()).map_err(|_| nfsstat3::NFS3ERR_INVAL)?,
        );
        match self.inner.lookup(NodeId(dirid), name) {
            Ok(Some(entry)) => Ok(entry.node.0),
            Ok(None) => Err(nfsstat3::NFS3ERR_NOENT),
            Err(e) => Err(mount_err_to_nfs(&e)),
        }
    }

    async fn getattr(&self, id: fileid3) -> std::result::Result<fattr3, nfsstat3> {
        let attrs = self
            .inner
            .attrs(NodeId(id))
            .map_err(|e| mount_err_to_nfs(&e))?;
        Ok(fattr_from(
            id,
            attrs.kind,
            attrs.size,
            attrs.unix_mode,
            attrs.nlink,
            attrs.mtime,
        ))
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> std::result::Result<fattr3, nfsstat3> {
        // Mode/uid/gid/atime/mtime: silently dropped. These are
        // pure metadata edits with no observable effect on the
        // captured tree, and refusing them would break the typical
        // editor save flow (vim, etc. SETATTR-then-WRITE).
        //
        // **Size** changes are the dangerous case and we reject
        // them explicitly. NFS clients commonly issue
        // `SETATTR size=0` immediately before saving a shorter
        // version of a file ("truncate-then-write"). Pre-fix this
        // handler returned `Ok` for the truncation, then the
        // follow-up `write(offset=0, shorter_bytes)` seeded the
        // node from the *old* blob and left the old file's tail
        // bytes hanging off the end — silent data corruption that
        // any editor save could trigger.
        //
        // The mount's `PlatformShell` doesn't yet have a truncate
        // primitive, so we have no way to honour the request
        // correctly. Rejecting `NFS3ERR_NOTSUPP` makes the
        // failure loud and forces the client into a delete+create
        // path (or surfaces a clear error to the user); that's
        // strictly better than silently producing wrong bytes in
        // the CAS.
        if let nfsserve::nfs::set_size3::size(requested) = setattr.size {
            let current = self
                .inner
                .attrs(NodeId(id))
                .map_err(|e| mount_err_to_nfs(&e))?
                .size;
            if requested != current {
                tracing::warn!(
                    node = id,
                    requested,
                    current,
                    "nfs: rejecting setattr size change — truncation not yet supported in shell"
                );
                return Err(nfsstat3::NFS3ERR_NOTSUPP);
            }
        }
        self.getattr(id).await
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> std::result::Result<(Vec<u8>, bool), nfsstat3> {
        let attrs = self
            .inner
            .attrs(NodeId(id))
            .map_err(|e| mount_err_to_nfs(&e))?;
        let size = attrs.size;
        // Clamp the request to the file's actual size so we
        // return the correct EOF flag.
        let end = offset.saturating_add(count as u64).min(size);
        let want = end.saturating_sub(offset);
        let mut buf = vec![0u8; want as usize];
        if want > 0 {
            let n = self
                .inner
                .read(NodeId(id), offset, &mut buf)
                .map_err(|e| mount_err_to_nfs(&e))?;
            buf.truncate(n);
        }
        let eof = end >= size;
        Ok((buf, eof))
    }

    async fn write(
        &self,
        id: fileid3,
        offset: u64,
        data: &[u8],
    ) -> std::result::Result<fattr3, nfsstat3> {
        self.inner
            .write(NodeId(id), offset, data)
            .map_err(|e| mount_err_to_nfs(&e))?;
        self.getattr(id).await
    }

    async fn create(
        &self,
        _dirid: fileid3,
        _filename: &filename3,
        _attr: sattr3,
    ) -> std::result::Result<(fileid3, fattr3), nfsstat3> {
        // New-file creation isn't representable through the
        // PlatformShell trait yet. See the trait's "Platform notes"
        // and the trait-extension discussion in
        // `docs/design/mount.md` (if/when it lands).
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn create_exclusive(
        &self,
        _dirid: fileid3,
        _filename: &filename3,
    ) -> std::result::Result<fileid3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn mkdir(
        &self,
        _dirid: fileid3,
        _dirname: &filename3,
    ) -> std::result::Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn remove(
        &self,
        _dirid: fileid3,
        _filename: &filename3,
    ) -> std::result::Result<(), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn rename(
        &self,
        _from_dirid: fileid3,
        _from_filename: &filename3,
        _to_dirid: fileid3,
        _to_filename: &filename3,
    ) -> std::result::Result<(), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> std::result::Result<ReadDirResult, nfsstat3> {
        let entries = self
            .inner
            .enumerate(NodeId(dirid))
            .map_err(|e| mount_err_to_nfs(&e))?;

        // Skip past start_after if non-zero. The protocol treats
        // start_after as opaque-but-monotonic; we use fileid as
        // the cursor.
        let mut produced: Vec<DirEntry> = Vec::new();
        let mut started = start_after == 0;
        for entry in entries.iter() {
            if !started {
                if entry.node.0 == start_after {
                    started = true;
                }
                continue;
            }
            if produced.len() >= max_entries {
                break;
            }
            let attrs = self
                .inner
                .attrs(entry.node)
                .map_err(|e| mount_err_to_nfs(&e))?;
            produced.push(DirEntry {
                fileid: entry.node.0,
                name: filename3::from(entry.name.as_encoded_bytes().to_vec()),
                attr: fattr_from(
                    entry.node.0,
                    entry.kind,
                    entry.size,
                    entry.unix_mode,
                    attrs.nlink,
                    attrs.mtime,
                ),
            });
        }
        let end = produced.len() < max_entries
            || (start_after == 0 && produced.len() == entries.len())
            || produced
                .last()
                .map(|last| {
                    entries
                        .last()
                        .map(|e| e.node.0 == last.fileid)
                        .unwrap_or(false)
                })
                .unwrap_or(false);
        Ok(ReadDirResult {
            entries: produced,
            end,
        })
    }

    async fn symlink(
        &self,
        _dirid: fileid3,
        _linkname: &filename3,
        _symlink: &nfspath3,
        _attr: &sattr3,
    ) -> std::result::Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn readlink(&self, id: fileid3) -> std::result::Result<nfspath3, nfsstat3> {
        // Heddle stores symlinks as blobs whose content is the
        // link target path, and `ContentAddressedMount::read`
        // already serves both regular files and symlinks out of the
        // same blob-backed code path. So `readlink` is just
        // `attrs → check kind → read whole blob → wrap as nfspath3`.
        //
        // Bound the buffer at the path-length cap most kernels
        // accept (4 KiB, matching `PATH_MAX` on macOS/Linux). A
        // captured symlink whose target exceeds that wouldn't be
        // round-trippable through the kernel anyway; surface the
        // overflow as `NFS3ERR_NAMETOOLONG`.
        let attrs = self
            .inner
            .attrs(NodeId(id))
            .map_err(|e| mount_err_to_nfs(&e))?;
        if !matches!(attrs.kind, NodeKind::Symlink) {
            return Err(nfsstat3::NFS3ERR_INVAL);
        }
        const MAX_SYMLINK_BYTES: u64 = 4096;
        if attrs.size > MAX_SYMLINK_BYTES {
            tracing::warn!(
                node = id,
                size = attrs.size,
                "nfs: symlink target exceeds PATH_MAX-class bound"
            );
            return Err(nfsstat3::NFS3ERR_NAMETOOLONG);
        }
        let mut buf = vec![0u8; attrs.size as usize];
        let n = self
            .inner
            .read(NodeId(id), 0, &mut buf)
            .map_err(|e| mount_err_to_nfs(&e))?;
        buf.truncate(n);
        Ok(nfspath3 { 0: buf })
    }
}

// ----------------------------------------------------------------
// Helpers
// ----------------------------------------------------------------

fn fattr_from(
    fileid: fileid3,
    kind: NodeKind,
    size: u64,
    unix_mode: u32,
    nlink: u32,
    mtime: SystemTime,
) -> fattr3 {
    let ftype = match kind {
        NodeKind::Directory => ftype3::NF3DIR,
        NodeKind::File => ftype3::NF3REG,
        NodeKind::Symlink => ftype3::NF3LNK,
    };
    let nfstime = system_time_to_nfstime(mtime);
    fattr3 {
        ftype,
        mode: (unix_mode & 0o7777) as mode3,
        nlink,
        uid: 0,
        gid: 0,
        size,
        used: size,
        rdev: specdata3::default(),
        fsid: 0,
        fileid,
        atime: nfstime,
        mtime: nfstime,
        ctime: nfstime,
    }
}

fn system_time_to_nfstime(t: SystemTime) -> nfstime3 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => nfstime3 {
            seconds: d.as_secs() as u32,
            nseconds: d.subsec_nanos(),
        },
        Err(_) => nfstime3 {
            seconds: 0,
            nseconds: 0,
        },
    }
}

fn mount_err_to_nfs(err: &MountError) -> nfsstat3 {
    // Write-side variants joined the enum with heddle#180; this
    // map gained the matching arms during heddle#190 once the
    // CLI's `--features mount` path started getting exercised
    // through the FUSE-worker dispatch.
    match err {
        MountError::NotFound(_) | MountError::UnknownThread(_) => nfsstat3::NFS3ERR_NOENT,
        MountError::Stale(_) => nfsstat3::NFS3ERR_STALE,
        MountError::NotADirectory(_) => nfsstat3::NFS3ERR_NOTDIR,
        MountError::ReadOnly => nfsstat3::NFS3ERR_ROFS,
        MountError::AlreadyExists(_) => nfsstat3::NFS3ERR_EXIST,
        MountError::IsADirectory(_) => nfsstat3::NFS3ERR_ISDIR,
        MountError::NotEmpty(_) => nfsstat3::NFS3ERR_NOTEMPTY,
        MountError::InvalidArgument(_) => nfsstat3::NFS3ERR_INVAL,
        // Session construction happens before the NFS server ever
        // dispatches a request, so this can't surface mid-protocol;
        // map it like any other infrastructure failure.
        MountError::SessionInit(_) => nfsstat3::NFS3ERR_IO,
        MountError::Store(_) => nfsstat3::NFS3ERR_IO,
    }
}

// ----------------------------------------------------------------
// Tests
// ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_runtime_available_is_always_true() {
        // The NFS fallback is supposed to be the universal
        // safety net — it doesn't probe for kernel modules or
        // optional features at the Rust layer (those failures
        // surface from `mount(8)` instead).
        assert!(NfsShell::is_runtime_available());
    }

    #[test]
    fn mount_err_to_nfs_maps_known_variants() {
        assert!(matches!(
            mount_err_to_nfs(&MountError::NotFound("x".into())),
            nfsstat3::NFS3ERR_NOENT
        ));
        assert!(matches!(
            mount_err_to_nfs(&MountError::ReadOnly),
            nfsstat3::NFS3ERR_ROFS
        ));
        assert!(matches!(
            mount_err_to_nfs(&MountError::NotADirectory("d".into())),
            nfsstat3::NFS3ERR_NOTDIR
        ));
    }
}
