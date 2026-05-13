// SPDX-License-Identifier: Apache-2.0
#[cfg(windows)]
use std::{
    ffi::OsString,
    os::windows::{ffi::OsStringExt, fs::MetadataExt},
    path::PathBuf,
};
#[cfg(unix)]
use std::{
    ffi::{CStr, CString, OsStr},
    os::{
        fd::{AsRawFd, RawFd},
        unix::ffi::OsStrExt,
    },
    ptr,
};
use std::{fs, io, path::Path};

#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY, FILE_SHARE_READ, FILE_SHARE_WRITE,
        GetFinalPathNameByHandleW, OPEN_EXISTING,
    },
};

#[cfg(unix)]
pub fn remove_path_recursively(path: &Path) -> io::Result<()> {
    remove_path_recursively_unix(path)
}

#[cfg(windows)]
pub fn remove_path_recursively(path: &Path) -> io::Result<()> {
    remove_path_recursively_windows(path)
}

#[cfg(not(any(unix, windows)))]
pub fn remove_path_recursively(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    let file_type = metadata.file_type();

    if !file_type.is_dir() {
        return fs::remove_file(path);
    }

    for entry in fs::read_dir(path)? {
        let entry = entry?;
        remove_path_recursively(&entry.path())?;
    }

    fs::remove_dir(path)
}

#[cfg(windows)]
fn remove_path_recursively_windows(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    let file_type = metadata.file_type();
    let is_reparse_point = metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;

    if !file_type.is_dir() {
        return fs::remove_file(path);
    }

    if is_reparse_point {
        return fs::remove_dir(path);
    }

    let dir = open_directory_handle(path)?;
    let stable_path = final_path_from_handle(dir.raw())?;

    for entry in fs::read_dir(&stable_path)? {
        let entry = entry?;
        remove_path_recursively_windows(&entry.path())?;
    }

    fs::remove_dir(stable_path)
}

#[cfg(windows)]
fn open_directory_handle(path: &Path) -> io::Result<OwnedWindowsHandle> {
    let wide = path_to_wide(path);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(OwnedWindowsHandle(handle))
    }
}

#[cfg(windows)]
fn final_path_from_handle(handle: HANDLE) -> io::Result<PathBuf> {
    let mut buffer = vec![0u16; 32768];
    let len =
        unsafe { GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), buffer.len() as u32, 0) };
    if len == 0 {
        return Err(io::Error::last_os_error());
    }

    let path = OsString::from_wide(&buffer[..len as usize]);
    let stable = PathBuf::from(path);
    Ok(stable)
}

#[cfg(windows)]
fn path_to_wide(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(windows)]
struct OwnedWindowsHandle(HANDLE);

#[cfg(windows)]
impl OwnedWindowsHandle {
    fn raw(&self) -> HANDLE {
        self.0
    }
}

#[cfg(windows)]
impl Drop for OwnedWindowsHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[cfg(unix)]
fn remove_path_recursively_unix(path: &Path) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("cannot remove root path {}", path.display()),
        )
    })?;

    let parent_dir = fs::File::open(parent)?;
    let entry_name = cstring_from_os_str(name)?;
    remove_path_recursively_at(parent_dir.as_raw_fd(), &entry_name)
}

#[cfg(unix)]
fn cstring_from_os_str(path: &OsStr) -> io::Result<CString> {
    CString::new(path.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path contains interior NUL: {}", Path::new(path).display()),
        )
    })
}

#[cfg(unix)]
fn remove_path_recursively_at(parent_fd: RawFd, entry_name: &CStr) -> io::Result<()> {
    let metadata = stat_no_follow(parent_fd, entry_name)?;
    let is_dir = (metadata.st_mode & libc::S_IFMT) == libc::S_IFDIR;

    if !is_dir {
        return unlink_at(parent_fd, entry_name, 0);
    }

    let child_fd = open_directory(parent_fd, entry_name)?;
    let dir = DirHandle::from_fd(child_fd)?;
    let dir_fd = dir.fd();

    while let Some(child_name) = dir.read_entry_name()? {
        if child_name.to_bytes() == b"." || child_name.to_bytes() == b".." {
            continue;
        }

        remove_path_recursively_at(dir_fd, child_name)?;
    }

    drop(dir);
    unlink_at(parent_fd, entry_name, libc::AT_REMOVEDIR)
}

#[cfg(unix)]
fn stat_no_follow(parent_fd: RawFd, entry_name: &CStr) -> io::Result<libc::stat> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    let rc = unsafe {
        libc::fstatat(
            parent_fd,
            entry_name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };

    if rc == 0 {
        Ok(unsafe { metadata.assume_init() })
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn open_directory(parent_fd: RawFd, entry_name: &CStr) -> io::Result<RawFd> {
    let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW;
    let fd = unsafe { libc::openat(parent_fd, entry_name.as_ptr(), flags) };
    if fd >= 0 {
        Ok(fd)
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn unlink_at(parent_fd: RawFd, entry_name: &CStr, flags: libc::c_int) -> io::Result<()> {
    let rc = unsafe { libc::unlinkat(parent_fd, entry_name.as_ptr(), flags) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
unsafe fn errno_ptr() -> *mut libc::c_int {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        unsafe { libc::__errno_location() }
    }

    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    {
        unsafe { libc::__error() }
    }
}

#[cfg(unix)]
struct DirHandle(*mut libc::DIR);

#[cfg(unix)]
impl DirHandle {
    fn from_fd(fd: RawFd) -> io::Result<Self> {
        let dir = unsafe { libc::fdopendir(fd) };
        if dir.is_null() {
            let err = io::Error::last_os_error();
            unsafe {
                libc::close(fd);
            }
            Err(err)
        } else {
            Ok(Self(dir))
        }
    }

    fn fd(&self) -> RawFd {
        unsafe { libc::dirfd(self.0) }
    }

    fn read_entry_name(&self) -> io::Result<Option<&CStr>> {
        unsafe {
            ptr::write(errno_ptr(), 0);
        }

        let entry = unsafe { libc::readdir(self.0) };
        if entry.is_null() {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(0) {
                Ok(None)
            } else {
                Err(err)
            }
        } else {
            Ok(Some(unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }))
        }
    }
}

#[cfg(unix)]
impl Drop for DirHandle {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_nested_directories_without_remove_dir_all() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("tree");
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("nested/file.txt"), b"hello").unwrap();

        remove_path_recursively(&root).unwrap();

        assert!(!root.exists());
    }

    #[cfg(unix)]
    #[test]
    fn removes_symlink_without_following_target() {
        let temp = tempfile::TempDir::new().unwrap();
        let target_dir = temp.path().join("target");
        let link_path = temp.path().join("link");
        fs::create_dir_all(&target_dir).unwrap();
        fs::write(target_dir.join("file.txt"), b"keep").unwrap();
        std::os::unix::fs::symlink(&target_dir, &link_path).unwrap();

        remove_path_recursively(&link_path).unwrap();

        assert!(!link_path.exists());
        assert!(target_dir.exists());
        assert!(target_dir.join("file.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn removes_fifo_nodes() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("tree");
        fs::create_dir_all(&root).unwrap();
        let fifo_path = root.join("daemon.fifo");
        let fifo_name = CString::new(fifo_path.as_os_str().as_bytes()).unwrap();

        let rc = unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) };
        assert_eq!(
            rc,
            0,
            "mkfifo should succeed: {}",
            io::Error::last_os_error()
        );

        remove_path_recursively(&root).unwrap();

        assert!(!root.exists());
        assert!(!fifo_path.exists());
    }
}