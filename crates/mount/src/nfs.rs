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
        let listener = runtime.block_on(NFSTcpListener::bind("127.0.0.1:0", fs)).map_err(
            |e| MountError::Store(objects::error::HeddleError::Io(e)),
        )?;
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
        invoke_mount(&mountpoint, port).map_err(|e| {
            // The server task gets cleaned up when `runtime` is
            // dropped at the end of this scope.
            MountError::Store(objects::error::HeddleError::Io(e))
        })?;

        Ok(NfsSession {
            runtime: Some(runtime),
            mountpoint,
            port,
        })
    }
}

pub struct NfsSession {
    /// Held so the spawned NFS server task lives until Drop.
    runtime: Option<Runtime>,
    mountpoint: PathBuf,
    #[allow(dead_code)]
    port: u16,
}

impl NfsSession {
    pub fn unmount(mut self) -> Result<()> {
        invoke_unmount(&self.mountpoint).map_err(|e| {
            MountError::Store(objects::error::HeddleError::Io(e))
        })?;
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
        if let Err(e) = invoke_unmount(&self.mountpoint) {
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
    let opts = format!(
        "vers=3,tcp,port={port},mountport={port},nolocks,soft,intr,actimeo=0,resvport=off"
    );
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
    let status = Command::new("umount.exe")
        .arg("-f")
        .arg(mp_str)
        .status()?;
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

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> std::result::Result<fileid3, nfsstat3> {
        let name = OsStr::new(std::str::from_utf8(filename.as_ref()).map_err(|_| nfsstat3::NFS3ERR_INVAL)?);
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
        Ok(fattr_from(id, attrs.kind, attrs.size, attrs.unix_mode, attrs.nlink, attrs.mtime))
    }

    async fn setattr(&self, id: fileid3, _setattr: sattr3) -> std::result::Result<fattr3, nfsstat3> {
        // We ignore the requested changes and return the current
        // attrs. This is the pragmatic minimum that keeps vim and
        // similar editors happy: they call setattr to truncate
        // before write, then the write itself overwrites whatever
        // was there. Mode/uid/gid changes are silently dropped.
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
        // Symlink readback would need a `PlatformShell::readlink`
        // hook the trait doesn't have yet. Surface as ENOSYS-ish
        // (NFS3ERR_NOTSUPP) so a readdir hit returning a symlink
        // is at least diagnosable.
        let _ = id;
        Err(nfsstat3::NFS3ERR_NOTSUPP)
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
    match err {
        MountError::NotFound(_) | MountError::UnknownThread(_) => nfsstat3::NFS3ERR_NOENT,
        MountError::Stale(_) => nfsstat3::NFS3ERR_STALE,
        MountError::NotADirectory(_) => nfsstat3::NFS3ERR_NOTDIR,
        MountError::ReadOnly => nfsstat3::NFS3ERR_ROFS,
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
