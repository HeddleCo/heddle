// SPDX-License-Identifier: Apache-2.0
//! Liveness probe for the mount-daemon Unix socket.
//!
//! Mode-0600 makes `connect` return `PermissionDenied` to every
//! non-owner. That is not proof the daemon is dead — the parent
//! dir may still allow unlink.

use std::{io::ErrorKind, os::unix::net::UnixStream, path::Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnixSocketProbe {
    Live,
    Dead,
    Inaccessible,
}

pub(crate) fn unix_socket_is_live(path: &Path) -> bool {
    matches!(probe_unix_socket(path), UnixSocketProbe::Live)
}

pub(crate) fn probe_unix_socket(path: &Path) -> UnixSocketProbe {
    match UnixStream::connect(path) {
        Ok(_) => UnixSocketProbe::Live,
        Err(error) => classify_unix_socket_connect(path, &error),
    }
}

/// Unlink only when the probe proves the path is dead.
pub(crate) fn classify_unix_socket_connect(path: &Path, error: &std::io::Error) -> UnixSocketProbe {
    use std::os::unix::fs::FileTypeExt;

    match error.kind() {
        ErrorKind::PermissionDenied => UnixSocketProbe::Inaccessible,
        ErrorKind::ConnectionRefused | ErrorKind::NotFound => UnixSocketProbe::Dead,
        _ => match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_socket() => UnixSocketProbe::Inaccessible,
            Ok(_) => UnixSocketProbe::Dead,
            Err(meta_error) if meta_error.kind() == ErrorKind::NotFound => UnixSocketProbe::Dead,
            Err(_) => UnixSocketProbe::Inaccessible,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::{io::ErrorKind, path::Path};

    use super::{classify_unix_socket_connect, UnixSocketProbe};

    #[test]
    fn permission_denied_probe_is_not_dead() {
        assert_eq!(
            classify_unix_socket_connect(
                Path::new("/unused"),
                &std::io::Error::from(ErrorKind::PermissionDenied),
            ),
            UnixSocketProbe::Inaccessible
        );
        assert_eq!(
            classify_unix_socket_connect(
                Path::new("/unused"),
                &std::io::Error::from(ErrorKind::ConnectionRefused),
            ),
            UnixSocketProbe::Dead
        );
    }
}
