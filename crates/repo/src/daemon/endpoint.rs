// SPDX-License-Identifier: Apache-2.0
//! On-disk endpoint discovery state for helper subprocesses.
//!
//! Every helper writes a small JSON file under `.heddle/state/`
//! advertising its `host:port` and the PID of the listening
//! process. CLI invocations read it, send a request, and decide
//! based on `kill -0 <pid>` whether the helper is still alive.

use std::{
    fs,
    path::{Path, PathBuf},
};

use objects::error::HeddleError;
use serde::{Deserialize, Serialize};

/// Persisted endpoint advertisement for a helper subprocess.
///
/// Versioning lives at the protocol layer (per-helper); this struct
/// only carries enough to *find* the helper. A `pid` field was added
/// so callers can probe for crashed daemons via `kill -0`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EndpointState {
    /// Wire protocol version the daemon speaks. CLI bumps this in
    /// lockstep with the daemon; a mismatch tells the CLI to remove
    /// the file and respawn.
    pub version: u32,
    pub host: String,
    pub port: u16,
    /// PID of the listening process. Optional for backward-compat
    /// with v1 fsmonitor endpoint files written before this field
    /// existed; treated as "unknown, assume alive" when absent.
    #[serde(default)]
    pub pid: Option<u32>,
}

/// Default state directory under a Heddle repo root:
/// `<repo_root>/.heddle/state`.
pub fn default_state_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".heddle/state")
}

/// Convention for endpoint file location:
/// `<repo_root>/.heddle/state/<name>.endpoint.json`.
pub fn endpoint_path_for(repo_root: &Path, name: &str) -> PathBuf {
    default_state_dir(repo_root).join(format!("{name}.endpoint.json"))
}

pub fn load_endpoint(path: &Path) -> Result<EndpointState, HeddleError> {
    let contents = fs::read_to_string(path)?;
    serde_json::from_str(&contents)
        .map_err(|error| HeddleError::Config(format!("decode helper endpoint: {error}")))
}

pub fn persist_endpoint(path: &Path, endpoint: &EndpointState) -> Result<(), HeddleError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let contents = serde_json::to_vec_pretty(endpoint)
        .map_err(|error| HeddleError::Config(format!("encode helper endpoint: {error}")))?;
    objects::fs_atomic::write_file_atomic(path, &contents)?;
    Ok(())
}

pub fn remove_endpoint(path: &Path) {
    let _ = fs::remove_file(path);
}

/// Remove an endpoint only when it still advertises the expected owner.
/// Callers must serialize endpoint writers across this check and unlink.
pub fn remove_endpoint_if_owned(path: &Path, expected: &EndpointState) -> bool {
    if load_endpoint(path).ok().as_ref() != Some(expected) {
        return false;
    }
    fs::remove_file(path).is_ok()
}

/// Best-effort liveness probe for a helper PID. Returns `true` when
/// the PID is alive, `false` when it definitely isn't, and `true`
/// when we genuinely cannot tell (no PID recorded → behave as if the
/// daemon is alive and let the connection attempt fail naturally).
///
/// On Unix we send signal 0 with [`libc::kill`]. The man page is
/// explicit: signal 0 performs the existence check without sending
/// a signal. ESRCH means "no such process". Anything else (EPERM
/// etc.) means a process exists with that PID even if we can't
/// signal it.
#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    // SAFETY: `kill` with signal 0 is the canonical Unix existence
    // probe — no memory effects, the only failure modes are ESRCH
    // (return false) and EPERM (return true; the process exists).
    let result = unsafe { libc_kill(pid as i32, 0) };
    if result == 0 {
        return true;
    }
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    // ESRCH = 3 on every Unix platform we ship on (Linux, macOS).
    // Anything else (EPERM, etc.) implies the process exists.
    errno != 3
}

#[cfg(not(unix))]
pub fn pid_alive(_pid: u32) -> bool {
    // On non-Unix we don't have a portable existence probe and
    // the daemon doesn't run there anyway. Conservative default:
    // assume alive; the connection attempt will fail naturally if
    // the helper is gone.
    true
}

#[cfg(unix)]
unsafe fn libc_kill(pid: i32, sig: i32) -> i32 {
    // Resolved via libc (workspace dep). Wrapped in a private fn to
    // keep the unsafe call surface small.
    unsafe { libc::kill(pid, sig) }
}

#[cfg(test)]
mod tests {
    //! Endpoint round-trip + stale-PID detection. These run on every
    //! supported host because the only platform-specific bit
    //! (`pid_alive` itself) is wrapped in `cfg(unix)` upstream.

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn endpoint_round_trips_through_disk() {
        let tmp = TempDir::new().unwrap();
        let path = endpoint_path_for(tmp.path(), "test");
        let original = EndpointState {
            version: 2,
            host: "127.0.0.1".to_string(),
            port: 9999,
            pid: Some(12345),
        };
        persist_endpoint(&path, &original).unwrap();
        let loaded = load_endpoint(&path).unwrap();
        assert_eq!(loaded, original);
    }

    #[test]
    fn endpoint_path_uses_state_dir_convention() {
        let tmp = TempDir::new().unwrap();
        let path = endpoint_path_for(tmp.path(), "heddled");
        assert!(path.ends_with(".heddle/state/heddled.endpoint.json"));
    }

    #[test]
    fn remove_endpoint_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let path = endpoint_path_for(tmp.path(), "ghost");
        // Removing a non-existent file should not panic or surface
        // an error to the caller.
        remove_endpoint(&path);
        remove_endpoint(&path);
    }

    #[test]
    fn owned_removal_preserves_a_replacement_endpoint() {
        let tmp = TempDir::new().unwrap();
        let path = endpoint_path_for(tmp.path(), "replacement");
        let old = EndpointState {
            version: 1,
            host: "127.0.0.1".to_string(),
            port: 8001,
            pid: Some(100),
        };
        let replacement = EndpointState {
            port: 8002,
            pid: Some(200),
            ..old.clone()
        };
        persist_endpoint(&path, &replacement).unwrap();

        assert!(!remove_endpoint_if_owned(&path, &old));
        assert_eq!(load_endpoint(&path).unwrap(), replacement);
        assert!(remove_endpoint_if_owned(&path, &replacement));
        assert!(!path.exists());
    }

    /// PID 1 is `init`/launchd — always alive on every Unix host. Use
    /// it as a sentinel "definitely alive" probe so we don't need to
    /// fork a process in the test.
    #[cfg(unix)]
    #[test]
    fn pid_alive_recognises_init() {
        assert!(pid_alive(1));
    }

    /// A PID we definitely don't own and that's overwhelmingly likely
    /// to be free on a single-user dev workstation. If this ever
    /// becomes flaky we can fork a child, capture its PID, wait it,
    /// and then probe — but in practice 32-bit-max PIDs aren't
    /// recycled and this is the same approach git uses for its
    /// fsmonitor liveness probe.
    #[cfg(unix)]
    #[test]
    fn pid_alive_returns_false_for_dead_pid() {
        // 0x7fff_fffe is just below i32::MAX; the kernel never
        // assigns PIDs that high in practice.
        assert!(!pid_alive(0x7fff_fffe));
    }
}
