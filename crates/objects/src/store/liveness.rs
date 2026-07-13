// SPDX-License-Identifier: Apache-2.0
//! Heartbeat-lease liveness for agent reservations.

use chrono::{DateTime, Duration, Utc};

pub const AGENT_LEASE_DURATION: Duration = Duration::minutes(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// The heartbeat lease is current and any recorded process is alive.
    Alive,
    /// The lease expired, the process exited, or the host rebooted.
    Dead,
}

impl std::fmt::Display for Liveness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Alive => write!(f, "alive"),
            Self::Dead => write!(f, "dead"),
        }
    }
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

/// Evaluate a heartbeat lease, using PID and boot identity as early death
/// signals when a long-lived owner process was explicitly recorded.
pub fn reservation_liveness_at(
    pid: Option<u32>,
    recorded_boot_id: Option<&str>,
    heartbeat_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Liveness {
    if pid.is_some_and(|pid| !process_alive(pid)) {
        return Liveness::Dead;
    }

    if matches!(
        (recorded_boot_id, current_boot_id()),
        (Some(recorded), Some(current)) if recorded != current
    ) {
        return Liveness::Dead;
    }

    match heartbeat_at {
        Some(heartbeat) if now <= heartbeat + AGENT_LEASE_DURATION => Liveness::Alive,
        Some(_) => Liveness::Dead,
        None => Liveness::Dead,
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
    fn reservation_is_alive_from_fresh_heartbeat_without_pid() {
        let now = Utc::now();
        assert_eq!(
            reservation_liveness_at(None, None, Some(now), now),
            Liveness::Alive
        );
    }

    #[test]
    fn reservation_is_dead_when_boot_id_mismatches() {
        let now = Utc::now();
        let pid = std::process::id();
        let liveness = reservation_liveness_at(
            Some(pid),
            Some("definitely-not-the-current-boot-id"),
            Some(now),
            now,
        );
        if current_boot_id().is_some() {
            assert_eq!(liveness, Liveness::Dead);
        } else {
            assert_eq!(liveness, Liveness::Alive);
        }
    }

    #[test]
    fn reservation_is_alive_when_lease_and_process_are_current() {
        let now = Utc::now();
        let pid = std::process::id();
        let boot = current_boot_id();
        assert_eq!(
            reservation_liveness_at(Some(pid), boot.as_deref(), Some(now), now),
            Liveness::Alive
        );
    }

    #[test]
    fn reservation_is_dead_when_pid_is_dead() {
        let now = Utc::now();
        let liveness = reservation_liveness_at(
            Some(0x7fff_ffff),
            current_boot_id().as_deref(),
            Some(now),
            now,
        );
        assert_eq!(liveness, Liveness::Dead);
    }

    #[test]
    fn reservation_is_dead_when_heartbeat_lease_expires() {
        let now = Utc::now();
        assert_eq!(
            reservation_liveness_at(
                None,
                None,
                Some(now - AGENT_LEASE_DURATION - Duration::seconds(1)),
                now,
            ),
            Liveness::Dead
        );
    }

    #[test]
    fn reservation_is_dead_without_heartbeat() {
        assert_eq!(
            reservation_liveness_at(None, None, None, Utc::now()),
            Liveness::Dead
        );
    }
}
