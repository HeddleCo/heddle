// SPDX-License-Identifier: Apache-2.0
//! Same-uid peer identity for Unix-domain sockets.
//!
//! The mount daemon used to bind unauthenticated localhost TCP.
//! `SO_PEERCRED` (Linux) / `getpeereid` (other Unix) is the uid
//! check the documented UDS path uses. Fail closed when credentials
//! cannot be read or the peer uid differs.

use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;

use objects::error::HeddleError;

/// True when `peer_uid` is this process's effective uid.
pub fn peer_uids_match(peer_uid: u32, self_uid: u32) -> bool {
    peer_uid == self_uid
}

/// Effective uid of this process. Used as the daemon-side identity.
pub fn current_euid() -> u32 {
    // SAFETY: `geteuid` is always successful and has no memory effects.
    unsafe { libc::geteuid() }
}

/// Fail closed unless the Unix-socket peer is the same effective uid.
pub fn check_peer_uid_matches_self(stream: &UnixStream) -> Result<(), HeddleError> {
    let peer_uid = peer_uid(stream)?;
    let self_uid = current_euid();
    if !peer_uids_match(peer_uid, self_uid) {
        return Err(HeddleError::Config(format!(
            "peer uid {peer_uid} does not match daemon uid {self_uid}"
        )));
    }
    Ok(())
}

fn peer_uid(stream: &UnixStream) -> Result<u32, HeddleError> {
    peer_uid_from_fd(stream.as_raw_fd())
}

#[cfg(target_os = "linux")]
fn peer_uid_from_fd(fd: libc::c_int) -> Result<u32, HeddleError> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `fd` is a live Unix socket; `cred`/`len` match SO_PEERCRED.
    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if ret != 0 {
        return Err(HeddleError::Config(format!(
            "SO_PEERCRED unavailable: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(cred.uid)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn peer_uid_from_fd(fd: libc::c_int) -> Result<u32, HeddleError> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: `fd` is a live Unix socket; getpeereid writes the two out-params.
    let ret = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
    if ret != 0 {
        return Err(HeddleError::Config(format!(
            "getpeereid unavailable: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(uid)
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;

    use super::{check_peer_uid_matches_self, current_euid, peer_uid, peer_uids_match};

    #[test]
    fn peer_uids_match_is_strict_equality() {
        assert!(peer_uids_match(1000, 1000));
        assert!(!peer_uids_match(0, 1000));
    }

    #[test]
    fn connected_pair_has_matching_self_uid() {
        let (left, right) = UnixStream::pair().expect("unix socket pair");
        let left_uid = peer_uid(&left).expect("peer uid on left");
        let right_uid = peer_uid(&right).expect("peer uid on right");
        let self_uid = current_euid();
        assert_eq!(left_uid, self_uid);
        assert_eq!(right_uid, self_uid);
        check_peer_uid_matches_self(&left).expect("same process is same uid");
        check_peer_uid_matches_self(&right).expect("same process is same uid");
    }
}
