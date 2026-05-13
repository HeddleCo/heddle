// SPDX-License-Identifier: Apache-2.0
//! Generic listener loop for helper daemons.
//!
//! Spawns nothing on its own — the caller binds the [`TcpListener`]
//! and provides a [`DaemonHandler`] for per-connection RPC + idle
//! ticks. We handle the accept loop, the idle-poll heartbeat, and
//! the per-connection JSON framing. The daemon-specific logic (verb
//! switch, registry mutation, persisted state) stays out of here.

use std::{
    io::{BufRead, BufReader, ErrorKind, Write},
    net::{TcpListener, TcpStream},
    time::{Duration, Instant},
};

use objects::error::HeddleError;

use super::protocol::{HELPER_IDLE_POLL_MS, HELPER_IDLE_TIMEOUT_SECS};

/// Decision returned by the per-tick policy when no connection is
/// pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleDecision {
    /// Loop again, sleep for the standard poll interval.
    Continue,
    /// Time to exit cleanly. The caller is expected to have removed
    /// the endpoint file before returning.
    Exit,
}

/// Per-daemon hook surface. The listener loop owns nothing daemon-
/// specific; instead it calls back into one of these on every
/// accepted connection or idle tick. This shape sidesteps the
/// double-borrow problem that comes with a pair of `FnMut` closures
/// that each capture the daemon state.
pub trait DaemonHandler {
    /// Called once per accepted TCP connection. Implementations are
    /// expected to read a single JSON line, dispatch to the verb
    /// handler, and write a JSON response. The shared
    /// [`handle_json_connection`] helper does this for daemons whose
    /// request/response types implement serde.
    fn handle(&mut self, stream: TcpStream) -> Result<(), HeddleError>;

    /// Called between accepts. Implementations may drain background
    /// state (e.g. fsmonitor's `notify` events) and decide whether
    /// the loop should continue or exit. `idle_for` is the duration
    /// since the last successful accept.
    fn on_tick(&mut self, idle_for: Duration) -> IdleDecision;
}

/// Drive `listener` with `handler` until the handler returns
/// [`IdleDecision::Exit`] from its tick. The listener must be
/// configured non-blocking by the caller.
pub fn run_server_loop<H: DaemonHandler>(
    listener: &TcpListener,
    handler: &mut H,
) -> Result<(), HeddleError> {
    let mut last_activity = Instant::now();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                last_activity = Instant::now();
                handler.handle(stream)?;
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                let elapsed = last_activity.elapsed();
                match handler.on_tick(elapsed) {
                    IdleDecision::Continue => {
                        std::thread::sleep(Duration::from_millis(HELPER_IDLE_POLL_MS));
                    }
                    IdleDecision::Exit => return Ok(()),
                }
            }
            Err(error) => return Err(HeddleError::Io(error)),
        }
    }
}

/// Default idle policy: exit when `elapsed >= HELPER_IDLE_TIMEOUT_SECS`.
/// fsmonitor uses this directly. The mount daemon composes this with
/// a "and no live mounts" gate via [`mount_idle_policy`]; the daemon
/// itself lives in `crates/cli/src/cli/commands/daemon/server.rs`.
pub fn default_idle_policy(elapsed: Duration) -> IdleDecision {
    if elapsed >= Duration::from_secs(HELPER_IDLE_TIMEOUT_SECS) {
        IdleDecision::Exit
    } else {
        IdleDecision::Continue
    }
}

/// Mount-daemon idle policy. Three inputs map to the three exits:
///
/// * `shutdown_requested` — operator asked the daemon to stop. Exit
///   immediately regardless of mount state; the caller is expected to
///   sweep mounts before returning.
/// * `live_mount_count` — number of FUSE sessions the daemon is
///   currently holding. Non-zero → keep going, regardless of idle.
/// * `idle_for` — duration since last RPC. Only consulted when the
///   registry is empty.
///
/// Pure function so the regression test ("idle exit must NOT fire
/// while a mount is live") can run on any host, not just Linux + FUSE.
pub fn mount_idle_policy(
    shutdown_requested: bool,
    live_mount_count: usize,
    idle_for: Duration,
) -> IdleDecision {
    if shutdown_requested {
        return IdleDecision::Exit;
    }
    if live_mount_count > 0 {
        return IdleDecision::Continue;
    }
    default_idle_policy(idle_for)
}

/// Read a newline-terminated JSON request from `stream`, hand it to
/// `respond`, and write the JSON response back. Used by both the
/// fsmonitor and mount handler dispatchers.
pub fn handle_json_connection<Req, Resp, Respond>(
    mut stream: TcpStream,
    respond: Respond,
) -> Result<(), HeddleError>
where
    Req: serde::de::DeserializeOwned,
    Resp: serde::Serialize,
    Respond: FnOnce(Req) -> Resp,
{
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let request: Req = serde_json::from_str(&line)
        .map_err(|error| HeddleError::Config(format!("decode helper request: {error}")))?;
    let response = respond(request);
    serde_json::to_writer(&mut stream, &response)
        .map_err(|error| HeddleError::Config(format!("encode helper response: {error}")))?;
    stream.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Idle-policy regression tests. These run on every supported
    //! host because the policy is a pure function — the fact that
    //! the daemon binary itself is Linux-only doesn't gate the
    //! correctness check.

    use std::time::Duration;

    use super::{IdleDecision, default_idle_policy, mount_idle_policy};
    use crate::daemon::HELPER_IDLE_TIMEOUT_SECS;

    #[test]
    fn fsmonitor_idle_policy_exits_at_timeout() {
        assert_eq!(
            default_idle_policy(Duration::from_secs(HELPER_IDLE_TIMEOUT_SECS)),
            IdleDecision::Exit
        );
        assert_eq!(
            default_idle_policy(Duration::from_secs(HELPER_IDLE_TIMEOUT_SECS - 1)),
            IdleDecision::Continue
        );
    }

    /// Regression test for the mount-daemon idle gate. Without this
    /// gate, a long-idle daemon would unmount the kernel mountpoint
    /// behind the user's back.
    #[test]
    fn mount_idle_policy_keeps_alive_while_mount_is_live() {
        // Way past the idle timeout, but a mount is live → keep going.
        let decision =
            mount_idle_policy(false, 1, Duration::from_secs(HELPER_IDLE_TIMEOUT_SECS * 10));
        assert_eq!(decision, IdleDecision::Continue);
    }

    #[test]
    fn mount_idle_policy_exits_when_registry_is_empty_after_timeout() {
        let decision =
            mount_idle_policy(false, 0, Duration::from_secs(HELPER_IDLE_TIMEOUT_SECS + 1));
        assert_eq!(decision, IdleDecision::Exit);
    }

    #[test]
    fn mount_idle_policy_continues_when_registry_is_empty_below_timeout() {
        let decision = mount_idle_policy(false, 0, Duration::from_secs(0));
        assert_eq!(decision, IdleDecision::Continue);
    }

    #[test]
    fn mount_idle_policy_exits_on_explicit_shutdown_even_with_live_mounts() {
        // Operator-requested shutdown overrides the live-mount gate.
        // The caller is responsible for draining mounts before exit.
        let decision = mount_idle_policy(true, 5, Duration::from_secs(0));
        assert_eq!(decision, IdleDecision::Exit);
    }
}