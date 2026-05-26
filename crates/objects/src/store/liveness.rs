// SPDX-License-Identifier: Apache-2.0
//! Process liveness detection for reservation reaping.
//!
//! `heddle agent reserve` is a one-shot command — the CLI process exits as
//! soon as the reservation has been recorded. Holding a per-session
//! `flock` for the life of that process therefore buys nothing: the
//! kernel releases the lock on `exit(2)` long before the next agent ever
//! needs to check liveness.
//!
//! We instead record `(pid, boot_id)` at reservation time and check
//! liveness on demand with `kill(pid, 0)` plus a boot-id comparison.
//! `ESRCH` means the process is gone. A boot id mismatch means the host
//! rebooted and the PID has been reused — the original owner is also
//! gone.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// The recorded process is still running on the current boot.
    Alive,
    /// The recorded process is gone (or the boot id has rolled).
    Dead,
    /// Insufficient information; default to leaving the entry alone so
    /// we never reap a live owner on missing fields.
    Unknown,
}

/// Best-effort current boot identifier.
///
/// - Linux: `/proc/sys/kernel/random/boot_id`.
/// - macOS: a stable prefix of `sysctl -n kern.boottime` (the `{ sec = …, usec = … }`
///   half is stable across invocations on the same boot; the trailing
///   human-readable date is not).
/// - Everything else: `None`.
#[cfg(target_os = "linux")]
pub fn current_boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(target_os = "macos")]
pub fn current_boot_id() -> Option<String> {
    std::process::Command::new("sysctl")
        .arg("-n")
        .arg("kern.boottime")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| {
            let trimmed = value.trim();
            let cutoff = trimmed
                .find('}')
                .map(|idx| idx + 1)
                .unwrap_or(trimmed.len());
            trimmed[..cutoff].to_string()
        })
        .filter(|value| !value.is_empty())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn current_boot_id() -> Option<String> {
    None
}

/// `true` if the process identified by `pid` is still running. ESRCH
/// from `kill(pid, 0)` is treated as dead. Any other error (notably
/// EPERM — the process exists but is owned by a different user) is
/// treated as alive: "alive in another uid namespace" still means the
/// reservation might be valid.
#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    let pid = pid as libc::pid_t;
    if pid <= 0 {
        return false;
    }
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return true;
    }
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    errno != libc::ESRCH
}

#[cfg(not(unix))]
pub fn process_alive(_pid: u32) -> bool {
    // Windows path — we don't have a kill(0) primitive without pulling
    // the Win32 process query in here. Default to Alive; the terminal-
    // status TTL remains the backstop.
    true
}

/// Combined check: PID is alive *and* the recorded boot id matches the
/// current boot id (when both are known). Missing fields collapse to
/// `Unknown` — callers should not reap on `Unknown`.
pub fn is_owner_alive(pid: Option<u32>, recorded_boot_id: Option<&str>) -> Liveness {
    let Some(pid) = pid else {
        return Liveness::Unknown;
    };

    if !process_alive(pid) {
        return Liveness::Dead;
    }

    match (recorded_boot_id, current_boot_id()) {
        (Some(recorded), Some(current)) if recorded != current => Liveness::Dead,
        _ => Liveness::Alive,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_alive_returns_true_for_self() {
        assert!(process_alive(std::process::id()));
    }

    #[test]
    fn process_alive_returns_false_for_pid_zero() {
        assert!(!process_alive(0));
    }

    #[test]
    fn process_alive_returns_false_for_unlikely_pid() {
        // PID 0x7fff_ffff is reserved on Linux and never assignable.
        // On macOS pids cap below 100k by default, so this is also
        // safely never-allocated. We accept the result for either case
        // since the test exists to ensure the ESRCH path is reachable.
        assert!(!process_alive(0x7fff_ffff));
    }

    #[test]
    fn is_owner_alive_unknown_without_pid() {
        assert_eq!(is_owner_alive(None, Some("boot")), Liveness::Unknown);
    }

    #[test]
    fn is_owner_alive_dead_when_boot_id_mismatches() {
        let pid = std::process::id();
        let liveness = is_owner_alive(Some(pid), Some("definitely-not-the-current-boot-id"));
        // If we can derive a real boot id on this platform the answer
        // is Dead; if we can't, the function falls through to Alive.
        if current_boot_id().is_some() {
            assert_eq!(liveness, Liveness::Dead);
        } else {
            assert_eq!(liveness, Liveness::Alive);
        }
    }

    #[test]
    fn is_owner_alive_alive_when_self_pid_and_matching_or_missing_boot_id() {
        let pid = std::process::id();
        let boot = current_boot_id();
        assert_eq!(is_owner_alive(Some(pid), boot.as_deref()), Liveness::Alive);
    }

    #[test]
    fn is_owner_alive_dead_when_pid_is_dead() {
        // PID 0x7fff_ffff is never assigned on Linux/macOS; treat as a
        // dead-pid proxy.
        let liveness = is_owner_alive(Some(0x7fff_ffff), current_boot_id().as_deref());
        assert_eq!(liveness, Liveness::Dead);
    }
}
