// SPDX-License-Identifier: Apache-2.0
//! A held directory capability for repository installation. All child names
//! are single components. Unix uses *at syscalls with no-follow opens; Windows
//! opens children relative to handles and pins every ancestor before using
//! write-through Win32 publication paths.
use std::{
    ffi::OsStr,
    fs::File,
    io,
    path::{Component, Path},
};

pub fn relative(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty() || !path.components().all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "installation paths must be relative, without traversal",
        ));
    }
    #[cfg(windows)]
    for component in path.components() {
        let value = component.as_os_str().to_string_lossy();
        let stem = value
            .split('.')
            .next()
            .unwrap_or_default()
            .trim_end_matches(' ')
            .to_ascii_uppercase();
        let device = matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
        ) || (["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|n| {
                matches!(
                    n,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        }));
        if value.contains(':') || value.ends_with(['.', ' ']) || device {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows aliases and alternate streams are not installation paths",
            ));
        }
    }
    Ok(())
}
fn name(name: &OsStr) -> io::Result<&Path> {
    let path = Path::new(name);
    relative(path)?;
    if path.components().count() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected one child name",
        ));
    }
    Ok(path)
}

pub struct Directory {
    file: File,
    #[cfg(windows)]
    path: std::path::PathBuf,
    #[cfg(windows)]
    ancestors: Vec<File>,
    #[cfg(windows)]
    pin: std::sync::Arc<File>,
}
impl Directory {
    pub fn open(path: &Path) -> io::Result<Self> {
        platform::open(path)
    }
    pub fn child(&self, child: &OsStr) -> io::Result<Self> {
        platform::child(self, name(child)?)
    }
    pub fn descend(&self, path: &Path) -> io::Result<Self> {
        let mut directory = self.try_clone()?;
        if !path.as_os_str().is_empty() {
            relative(path)?;
            for component in path.components() {
                directory = directory.child(component.as_os_str())?;
            }
        }
        Ok(directory)
    }
    pub fn try_clone(&self) -> io::Result<Self> {
        platform::clone(self)
    }
    pub fn identity(&self) -> io::Result<Vec<u8>> {
        platform::identity(&self.file)
    }
    pub fn open_file(&self, child: &OsStr) -> io::Result<File> {
        platform::file(self, name(child)?, false)
    }
    pub fn create_file(&self, child: &OsStr) -> io::Result<File> {
        platform::file(self, name(child)?, true)
    }
    pub fn hard_link(&self, source: &OsStr, destination: &OsStr) -> io::Result<()> {
        platform::link(self, name(source)?, name(destination)?)
    }
    /// Source data must be flushed first. Persist both directory entries before
    /// return, independently of reconstructible-clone durability suppression.
    pub fn durable_rename(&self, source: &OsStr, destination: &OsStr) -> io::Result<()> {
        platform::rename(self, name(source)?, name(destination)?)
    }
    pub fn remove_file(&self, child: &OsStr) -> io::Result<()> {
        platform::remove(self, name(child)?)
    }
}

#[cfg(unix)]
mod platform {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{
                ffi::OsStrExt,
                fs::{MetadataExt, OpenOptionsExt},
            },
        },
    };

    use super::*;
    fn c(path: &Path) -> io::Result<CString> {
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))
    }
    fn result(rc: i32) -> io::Result<()> {
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    fn open_at(parent: &Directory, path: &Path, flags: i32) -> io::Result<File> {
        let path = c(path)?;
        // SAFETY: live directory descriptor and NUL-terminated child name.
        let fd = unsafe {
            libc::openat(
                parent.file.as_raw_fd(),
                path.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful openat transferred this descriptor to us.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    pub fn open(path: &Path) -> io::Result<Directory> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        Ok(Directory { file })
    }
    pub fn child(parent: &Directory, path: &Path) -> io::Result<Directory> {
        Ok(Directory {
            file: open_at(parent, path, libc::O_RDONLY | libc::O_DIRECTORY)?,
        })
    }
    pub fn clone(directory: &Directory) -> io::Result<Directory> {
        Ok(Directory {
            file: directory.file.try_clone()?,
        })
    }
    pub fn identity(file: &File) -> io::Result<Vec<u8>> {
        let meta = file.metadata()?;
        Ok([meta.dev().to_le_bytes(), meta.ino().to_le_bytes()].concat())
    }
    pub fn file(directory: &Directory, path: &Path, create: bool) -> io::Result<File> {
        let flags = if create {
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL
        } else {
            libc::O_RDONLY | libc::O_NONBLOCK
        };
        let file = open_at(directory, path, flags)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "artifact must be a regular file",
            ));
        }
        Ok(file)
    }
    pub fn link(dir: &Directory, source: &Path, destination: &Path) -> io::Result<()> {
        let source = c(source)?;
        let destination = c(destination)?;
        // SAFETY: held directory and validated, terminated child names. linkat
        // does not follow the source symlink.
        result(unsafe {
            libc::linkat(
                dir.file.as_raw_fd(),
                source.as_ptr(),
                dir.file.as_raw_fd(),
                destination.as_ptr(),
                0,
            )
        })
    }
    pub fn rename(dir: &Directory, source: &Path, destination: &Path) -> io::Result<()> {
        let source = c(source)?;
        let destination = c(destination)?;
        // SAFETY: both names are relative to the same live directory handle.
        result(unsafe {
            libc::renameat(
                dir.file.as_raw_fd(),
                source.as_ptr(),
                dir.file.as_raw_fd(),
                destination.as_ptr(),
            )
        })?;
        dir.file.sync_all()
    }
    pub fn remove(dir: &Directory, child: &Path) -> io::Result<()> {
        let child = c(child)?;
        // SAFETY: held directory handle and terminated child name.
        result(unsafe { libc::unlinkat(dir.file.as_raw_fd(), child.as_ptr(), 0) })?;
        dir.file.sync_all()
    }
}

#[cfg(windows)]
mod platform {
    use std::os::windows::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::{AsRawHandle, FromRawHandle},
    };

    use windows_sys::{
        Wdk::{
            Foundation::OBJECT_ATTRIBUTES,
            Storage::FileSystem::{
                FILE_CREATE, FILE_DELETE_ON_CLOSE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE,
                FILE_OPEN, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
            },
        },
        Win32::{
            Foundation::{
                OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE, RtlNtStatusToDosError, UNICODE_STRING,
            },
            Storage::FileSystem::{
                DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_ID_INFO,
                FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, FileIdInfo,
                GetFileInformationByHandleEx, SYNCHRONIZE,
            },
            System::IO::IO_STATUS_BLOCK,
        },
    };

    use super::*;
    fn directory_file(path: &Path) -> io::Result<File> {
        // Only the volume root is opened by path. Child renames open their
        // destination directory with FILE_ADD_FILE, so allow write sharing.
        // Deny delete sharing to keep the directory's own name pinned.
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES | FILE_TRAVERSE | SYNCHRONIZE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        validate_directory(&file)?;
        Ok(file)
    }
    fn validate_directory(file: &File) -> io::Result<()> {
        let meta = file.metadata()?;
        if !meta.is_dir() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "installation ancestor is a reparse point or not a directory",
            ));
        }
        Ok(())
    }
    fn relative_open(parent: &File, child: &Path, directory: bool, pin: bool) -> io::Result<File> {
        name(child.as_os_str())?;
        let mut wide: Vec<u16> = child.as_os_str().encode_wide().collect();
        let length = wide
            .len()
            .checked_mul(2)
            .and_then(|n| u16::try_from(n).ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "child name too long"))?;
        if wide.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "NUL in child name",
            ));
        }
        let object_name = UNICODE_STRING {
            Length: length,
            MaximumLength: length,
            Buffer: wide.as_mut_ptr(),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: parent.as_raw_handle(),
            ObjectName: &object_name,
            Attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
            ..Default::default()
        };
        let mut handle = std::ptr::null_mut();
        let mut status_block = IO_STATUS_BLOCK::default();
        let access = if pin {
            FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE
        } else {
            FILE_READ_ATTRIBUTES | FILE_TRAVERSE | SYNCHRONIZE
        };
        let options = FILE_OPEN_REPARSE_POINT
            | FILE_SYNCHRONOUS_IO_NONALERT
            | if directory {
                FILE_DIRECTORY_FILE
            } else {
                FILE_NON_DIRECTORY_FILE
            }
            | if pin { FILE_DELETE_ON_CLOSE } else { 0 };
        // SAFETY: live RootDirectory handle, validated single-component UTF-16
        // name and correctly sized output buffers. Synchronous mode cannot pend.
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                access,
                &attributes,
                &mut status_block,
                std::ptr::null(),
                0,
                // MS-FSA 2.1.5.15.12 opens the rename destination directory with
                // FILE_ADD_FILE. Share writes there, but not deletion of its
                // own name. The pin file must still deny writes and deletion.
                if directory {
                    FILE_SHARE_READ | FILE_SHARE_WRITE
                } else {
                    FILE_SHARE_READ
                },
                if pin { FILE_CREATE } else { FILE_OPEN },
                options,
                std::ptr::null(),
                0,
            )
        };
        if status < 0 {
            // SAFETY: conversion accepts every NTSTATUS value.
            return Err(io::Error::from_raw_os_error(
                unsafe { RtlNtStatusToDosError(status) } as i32,
            ));
        }
        // SAFETY: successful NtCreateFile transferred ownership of the handle.
        Ok(unsafe { File::from_raw_handle(handle) })
    }
    fn pinned(file: File, path: std::path::PathBuf, ancestors: Vec<File>) -> io::Result<Directory> {
        validate_directory(&file)?;
        let pin_path = crate::fs_atomic::temp_path(Path::new("hosted-dir-pin"));
        let pin_name = pin_path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing pin name"))?;
        let pin = relative_open(&file, Path::new(pin_name), false, true)?;
        validate_directory(&file)?;
        // With all names pinned, canonicalization cannot cross a substituted
        // ancestor. Keep a verbatim Win32 path so publication does not apply DOS
        // device parsing to names resolved by NtCreateFile.
        let path = path.canonicalize()?;
        // Denying delete sharing pins names, but attribute-only handles bypass
        // sharing restrictions. MS-FSA forbids setting reparse points on nonempty
        // directories: this undeletable file pins the leaf; held children pin
        // ancestors. Delete-on-close also survives process exit without a janitor.
        Ok(Directory {
            file,
            path,
            ancestors,
            pin: std::sync::Arc::new(pin),
        })
    }
    pub fn open(path: &Path) -> io::Result<Directory> {
        let path = std::path::absolute(path)?;
        let mut prefix = std::path::PathBuf::new();
        let mut ancestors = Vec::new();
        let mut current = None;
        for component in path.components() {
            prefix.push(component);
            if component == Component::RootDir {
                current = Some(directory_file(&prefix)?);
            } else if let Component::Normal(child) = component {
                let parent = current.take().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "missing volume root")
                })?;
                let next = relative_open(&parent, Path::new(child), true, false)?;
                validate_directory(&next)?;
                ancestors.push(parent);
                current = Some(next);
            }
            if matches!(component, Component::ParentDir | Component::CurDir) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid root path",
                ));
            }
        }
        let file = current
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing directory"))?;
        pinned(file, path, ancestors)
    }
    pub fn child(parent: &Directory, child: &Path) -> io::Result<Directory> {
        let path = parent.path.join(child);
        let mut ancestors = parent
            .ancestors
            .iter()
            .map(File::try_clone)
            .collect::<io::Result<Vec<_>>>()?;
        ancestors.push(parent.file.try_clone()?);
        let file = relative_open(&parent.file, child, true, false)?;
        pinned(file, path, ancestors)
    }
    pub fn clone(dir: &Directory) -> io::Result<Directory> {
        Ok(Directory {
            file: dir.file.try_clone()?,
            path: dir.path.clone(),
            pin: dir.pin.clone(),
            ancestors: dir
                .ancestors
                .iter()
                .map(File::try_clone)
                .collect::<io::Result<_>>()?,
        })
    }
    pub fn identity(file: &File) -> io::Result<Vec<u8>> {
        let mut info = std::mem::MaybeUninit::<FILE_ID_INFO>::uninit();
        // SAFETY: live file handle and correctly sized output buffer.
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                info.as_mut_ptr().cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful call initialized FILE_ID_INFO.
        let info = unsafe { info.assume_init() };
        Ok([
            info.VolumeSerialNumber.to_le_bytes().as_slice(),
            &info.FileId.Identifier,
        ]
        .concat())
    }
    pub fn file(dir: &Directory, child: &Path, create: bool) -> io::Result<File> {
        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        if create {
            options.write(true).create_new(true);
        }
        let file = options.open(dir.path.join(child))?;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "artifact must be a regular file",
            ));
        }
        Ok(file)
    }
    pub fn link(dir: &Directory, source: &Path, destination: &Path) -> io::Result<()> {
        std::fs::hard_link(dir.path.join(source), dir.path.join(destination))
    }
    pub fn rename(dir: &Directory, source: &Path, destination: &Path) -> io::Result<()> {
        crate::fs_atomic::durable_rename(&dir.path.join(source), &dir.path.join(destination))
    }
    pub fn remove(dir: &Directory, child: &Path) -> io::Result<()> {
        std::fs::remove_file(dir.path.join(child))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    #[cfg(windows)]
    #[test]
    fn windows_aliases_are_not_relative_installation_paths() {
        for value in [
            "NUL",
            "COM1.txt",
            "COM¹",
            "LPT².txt",
            "COM1 .txt",
            "CONIN$",
            "a:b",
            "a.",
        ] {
            assert!(relative(Path::new(value)).is_err(), "accepted {value}");
        }
        assert!(relative(Path::new("parent/pin")).is_ok());
    }
    #[test]
    fn flushed_publish_and_restore_use_durable_renames() {
        let root = tempfile::tempdir().expect("root");
        let directory = Directory::open(root.path()).expect("capability");
        for (name, bytes) in [("backup", b"old"), ("new", b"new")] {
            let mut file = directory.create_file(OsStr::new(name)).expect("create");
            file.write_all(bytes).expect("write");
            file.sync_all().expect("file flush");
            drop(file);
        }
        directory
            .durable_rename(OsStr::new("new"), OsStr::new("pin"))
            .expect("publish");
        assert_eq!(std::fs::read(root.path().join("pin")).expect("pin"), b"new");
        directory
            .hard_link(OsStr::new("backup"), OsStr::new("restore"))
            .expect("preserve undo");
        directory
            .durable_rename(OsStr::new("restore"), OsStr::new("pin"))
            .expect("restore");
        assert_eq!(std::fs::read(root.path().join("pin")).expect("pin"), b"old");
        assert!(root.path().join("backup").exists());
    }
    #[cfg(windows)]
    #[test]
    fn held_windows_ancestors_prevent_parent_replacement() {
        let root = tempfile::tempdir().expect("root");
        std::fs::create_dir_all(root.path().join("ancestor/parent")).expect("parent");
        let directory = Directory::open(root.path()).expect("root capability");
        let ancestor = directory
            .child(OsStr::new("ancestor"))
            .expect("ancestor capability");
        let parent = ancestor
            .child(OsStr::new("parent"))
            .expect("parent capability");
        drop(ancestor);
        drop(directory);
        let parent_path = root.path().join("ancestor/parent");
        assert!(std::fs::rename(&parent_path, root.path().join("moved-parent")).is_err());
        assert!(std::fs::rename(root.path().join("ancestor"), root.path().join("moved")).is_err());
        let mut file = parent.create_file(OsStr::new("new")).expect("held create");
        file.write_all(b"data").expect("write");
        file.sync_all().expect("non-privileged file flush");
        drop(file);
        parent
            .durable_rename(OsStr::new("new"), OsStr::new("pin"))
            .expect("non-privileged write-through move");
        assert_eq!(
            std::fs::read(parent_path.join("pin")).expect("pin"),
            b"data"
        );
        assert!(std::fs::rename(&parent_path, root.path().join("moved-parent")).is_err());
        assert!(std::fs::rename(root.path().join("ancestor"), root.path().join("moved")).is_err());
        drop(parent);
        std::fs::rename(&parent_path, root.path().join("moved-parent"))
            .expect("released leaf handle");
        std::fs::rename(root.path().join("ancestor"), root.path().join("moved"))
            .expect("released handles");
    }
    #[cfg(windows)]
    #[test]
    fn windows_attribute_only_junction_substitution_is_blocked() {
        use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle};

        use windows_sys::Win32::{
            Storage::FileSystem::{
                FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
                FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES,
            },
            System::{IO::DeviceIoControl, Ioctl::FSCTL_SET_REPARSE_POINT},
        };
        fn set_junction(directory: &Path, target: &Path) -> io::Result<()> {
            let print: Vec<u16> = target.as_os_str().encode_wide().collect();
            let substitute: Vec<u16> = format!("\\??\\{}", target.display())
                .encode_utf16()
                .collect();
            let mut data = Vec::new();
            data.extend_from_slice(&0xA0000003u32.to_le_bytes()); // mount-point tag
            data.extend_from_slice(
                &((8 + 2 * (substitute.len() + print.len() + 2)) as u16).to_le_bytes(),
            );
            data.extend_from_slice(&0u16.to_le_bytes());
            for value in [
                0,
                substitute.len() * 2,
                (substitute.len() + 1) * 2,
                print.len() * 2,
            ] {
                data.extend_from_slice(&(value as u16).to_le_bytes());
            }
            for value in substitute.into_iter().chain([0]).chain(print).chain([0]) {
                data.extend_from_slice(&value.to_le_bytes());
            }
            // Attribute-only access bypasses share-mode write restrictions.
            let file = std::fs::OpenOptions::new()
                .access_mode(FILE_WRITE_ATTRIBUTES)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(directory)?;
            let mut returned = 0;
            // SAFETY: live handle and a complete mount-point reparse buffer.
            if unsafe {
                DeviceIoControl(
                    file.as_raw_handle(),
                    FSCTL_SET_REPARSE_POINT,
                    data.as_ptr().cast(),
                    data.len() as u32,
                    std::ptr::null_mut(),
                    0,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }
        let root = tempfile::tempdir().expect("root");
        let outside = tempfile::tempdir().expect("outside");
        for name in ["control", "parent"] {
            std::fs::create_dir(root.path().join(name)).expect("directory");
        }
        set_junction(&root.path().join("control"), outside.path())
            .expect("non-privileged junction control");
        let directory = Directory::open(root.path()).expect("root capability");
        assert!(directory.child(OsStr::new("control")).is_err());
        let parent = directory.child(OsStr::new("parent")).expect("held parent");
        let error = set_junction(&root.path().join("parent"), outside.path())
            .expect_err("nonempty pin must prevent reparse substitution");
        assert_eq!(error.raw_os_error(), Some(145)); // ERROR_DIR_NOT_EMPTY
        let mut file = parent
            .create_file(OsStr::new("new"))
            .expect("contained create");
        file.write_all(b"inside").expect("write");
        file.sync_all().expect("flush");
        drop(file);
        parent
            .durable_rename(OsStr::new("new"), OsStr::new("artifact"))
            .expect("publish");
        assert!(!outside.path().join("artifact").exists());
        drop(parent);
        drop(directory);
        std::fs::remove_dir(root.path().join("control")).expect("remove control junction");
    }
}
