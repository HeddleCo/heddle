// SPDX-License-Identifier: Apache-2.0
//! Generic listener loop for helper daemons.
//!
//! Spawns nothing on its own — the caller binds the [`TcpListener`]
//! and provides a [`DaemonHandler`] for per-connection RPC + idle
//! ticks. We handle the accept loop, the idle heartbeat, and the
//! per-connection JSON framing. The daemon-specific logic (verb
//! switch, registry mutation, persisted state) stays out of here.
//!
//! The wait is event-driven. An earlier shape put the listener in
//! non-blocking mode and spun `accept()` / `sleep(5ms)`, which cost
//! ~60,000 `accept4`-EAGAIN + `clock_nanosleep` pairs over a single
//! helper's 300s idle lifetime — measured at 46,680 `accept4` (46,679
//! failing) and 46,678 `clock_nanosleep` in the heddle#1243 clone
//! trace. Now a dedicated thread parks in a blocking `accept()` and
//! hands sockets over a channel; the main loop parks on that channel
//! with [`DaemonHandler::tick_interval`] as its timeout.

use std::{
    io::{BufRead, BufReader, ErrorKind, Write},
    net::{TcpListener, TcpStream},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

use objects::error::HeddleError;

use super::protocol::{HELPER_IDLE_TIMEOUT_SECS, HELPER_TICK_INTERVAL_MS};

/// How long the exit path waits on its own listener when handing the
/// acceptor thread the connection that retires it. Bounded because
/// the dial is a courtesy, not a requirement — see the teardown note
/// in [`run_server_loop`].
const ACCEPTOR_RETIRE_TIMEOUT: Duration = Duration::from_millis(250);

/// Decision returned by the per-tick policy when no connection is
/// pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleDecision {
    /// Keep serving: park until the next connection lands or the
    /// tick interval elapses, whichever comes first.
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

    /// Called after every serviced connection and whenever
    /// [`DaemonHandler::tick_interval`] elapses with no connection.
    /// Implementations may drain background state (e.g. fsmonitor's
    /// `notify` events) and decide whether the loop should continue
    /// or exit. `idle_for` is the duration since the last accepted
    /// connection.
    fn on_tick(&mut self, idle_for: Duration) -> IdleDecision;

    /// How long to park waiting for a connection before running an
    /// idle tick anyway.
    ///
    /// This does not delay RPC service — a connection wakes the loop
    /// immediately. It only bounds how stale a handler's background
    /// state may get between requests, and how coarsely the idle
    /// timeout is observed.
    fn tick_interval(&self) -> Duration {
        Duration::from_millis(HELPER_TICK_INTERVAL_MS)
    }
}

/// Drive `listener` with `handler` until the handler returns
/// [`IdleDecision::Exit`] from its tick.
///
/// The listener is switched to blocking mode: a detached acceptor
/// thread owns the `accept()` call and forwards each socket over a
/// channel, so an idle helper costs one parked thread rather than a
/// poll-sleep loop.
pub fn run_server_loop<H: DaemonHandler>(
    listener: &TcpListener,
    handler: &mut H,
) -> Result<(), HeddleError> {
    listener.set_nonblocking(false)?;
    // Cloned so the acceptor owns a `'static` listener: the thread is
    // deliberately not joined (see the teardown note below), so it
    // cannot borrow from this frame.
    let local_addr = listener.local_addr()?;
    let acceptor_listener = listener.try_clone()?;
    let (connection_tx, connection_rx) = mpsc::channel::<TcpStream>();

    thread::spawn(move || {
        loop {
            match acceptor_listener.accept() {
                // A send error means the serve loop is gone; so are we.
                Ok((stream, _)) => {
                    if connection_tx.send(stream).is_err() {
                        return;
                    }
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    });

    let result = serve_connections(handler, &connection_rx);

    // Retire the acceptor, but never *wait* on it. The thread is
    // parked inside `accept()` and only notices the dropped receiver
    // once a connection lands, so we hand it one by dialling our own
    // loopback listener. That dial is best-effort: it is exactly the
    // operation that fails when the box is out of file descriptors,
    // and an earlier revision of this function joined the acceptor
    // afterwards — which turned a failed dial into a helper that
    // parks forever, holding its inotify instance and its endpoint
    // file. Detaching instead bounds the exit path unconditionally;
    // a still-parked acceptor dies with the process.
    drop(connection_rx);
    let _ = TcpStream::connect_timeout(&local_addr, ACCEPTOR_RETIRE_TIMEOUT);
    result
}

/// Service connections until the handler asks to exit. Split out of
/// [`run_server_loop`] so the acceptor teardown above runs on every
/// exit path, including the error one.
fn serve_connections<H: DaemonHandler>(
    handler: &mut H,
    connections: &Receiver<TcpStream>,
) -> Result<(), HeddleError> {
    let mut last_activity = Instant::now();
    loop {
        let decision = match connections.recv_timeout(handler.tick_interval()) {
            Ok(stream) => {
                last_activity = Instant::now();
                handler.handle(stream)?;
                // Tick straight after the RPC rather than waiting out
                // a whole interval: a `shutdown` verb sets its flag
                // during `handle`, and this is where the handler gets
                // to act on it.
                handler.on_tick(last_activity.elapsed())
            }
            Err(RecvTimeoutError::Timeout) => handler.on_tick(last_activity.elapsed()),
            // The acceptor gave up (listener error). Nothing more will
            // ever arrive, so shut down cleanly.
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        };
        if decision == IdleDecision::Exit {
            return Ok(());
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

    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        thread,
        time::{Duration, Instant},
    };

    use objects::error::HeddleError;

    use super::{
        DaemonHandler, IdleDecision, default_idle_policy, mount_idle_policy, run_server_loop,
    };
    use crate::daemon::{HELPER_HOST, HELPER_IDLE_TIMEOUT_SECS};

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

    /// Handler that records how many idle ticks it saw and exits once
    /// it has been alive for `lifetime`.
    struct TickCounter {
        ticks: usize,
        started: Instant,
        lifetime: Duration,
        tick: Duration,
    }

    impl DaemonHandler for TickCounter {
        fn handle(&mut self, _stream: TcpStream) -> Result<(), HeddleError> {
            Ok(())
        }

        fn on_tick(&mut self, _idle_for: Duration) -> IdleDecision {
            self.ticks += 1;
            if self.started.elapsed() >= self.lifetime {
                IdleDecision::Exit
            } else {
                IdleDecision::Continue
            }
        }

        fn tick_interval(&self) -> Duration {
            self.tick
        }
    }

    /// Regression test for heddle#1243: an idle helper must park, not
    /// spin. The tick count over a fixed idle window is the observable
    /// proxy for the syscall storm — the pre-fix loop woke every 5ms
    /// (an `accept4`-EAGAIN plus a `clock_nanosleep` each time), so
    /// this 500ms window would have produced ~100 ticks instead of ~5.
    #[test]
    fn an_idle_helper_ticks_on_its_interval_instead_of_spinning() {
        let listener = TcpListener::bind((HELPER_HOST, 0)).unwrap();
        let lifetime = Duration::from_millis(500);
        let tick = Duration::from_millis(100);
        let mut handler = TickCounter {
            ticks: 0,
            started: Instant::now(),
            lifetime,
            tick,
        };

        run_server_loop(&listener, &mut handler).unwrap();

        // The loop cannot tick faster than its interval, so the ceiling
        // is the window divided by the interval, plus slack for a
        // loaded CI box. A 5ms poll-sleep would blow straight past it.
        let ceiling = 2 * (lifetime.as_millis() / tick.as_millis()) as usize;
        assert!(
            handler.ticks <= ceiling,
            "idle helper ticked {} times in {lifetime:?} at a {tick:?} interval (ceiling {ceiling}) — the loop is spinning",
            handler.ticks
        );
        assert!(
            handler.ticks >= 2,
            "idle helper should still tick at all; saw {}",
            handler.ticks
        );
    }

    /// Handler that answers one connection and then exits.
    struct OneShot {
        served: bool,
        tick: Duration,
    }

    impl DaemonHandler for OneShot {
        fn handle(&mut self, mut stream: TcpStream) -> Result<(), HeddleError> {
            stream.write_all(b"ok\n")?;
            self.served = true;
            Ok(())
        }

        fn on_tick(&mut self, _idle_for: Duration) -> IdleDecision {
            if self.served {
                IdleDecision::Exit
            } else {
                IdleDecision::Continue
            }
        }

        fn tick_interval(&self) -> Duration {
            self.tick
        }
    }

    /// The other half of the fix: parking on a long interval must not
    /// delay RPC service. A connection has to wake the loop straight
    /// away, and the post-RPC tick has to observe the handler's own
    /// exit request without waiting out another interval.
    #[test]
    fn a_connection_is_served_without_waiting_out_the_tick_interval() {
        let listener = TcpListener::bind((HELPER_HOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        // Far longer than the test's own timeout: if service were gated
        // on the tick, this test would hang rather than merely fail.
        let tick = Duration::from_secs(300);

        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            let mut reply = String::new();
            stream.read_to_string(&mut reply).unwrap();
            reply
        });

        let started = Instant::now();
        let mut handler = OneShot {
            served: false,
            tick,
        };
        run_server_loop(&listener, &mut handler).unwrap();
        let elapsed = started.elapsed();

        assert_eq!(client.join().unwrap(), "ok\n");
        assert!(handler.served);
        assert!(
            elapsed < Duration::from_secs(30),
            "serving one RPC took {elapsed:?}; the loop is gating accepts on its tick interval"
        );
    }

    /// Handler that exits on its very first tick, without ever being
    /// handed a connection.
    struct ExitAtOnce;

    impl DaemonHandler for ExitAtOnce {
        fn handle(&mut self, _stream: TcpStream) -> Result<(), HeddleError> {
            Ok(())
        }

        fn on_tick(&mut self, _idle_for: Duration) -> IdleDecision {
            IdleDecision::Exit
        }

        fn tick_interval(&self) -> Duration {
            Duration::from_millis(10)
        }
    }

    /// The exit path must be bounded. It retires the acceptor thread by
    /// dialling the loop's own listener, and that dial can fail — it is
    /// the first thing to break when a box is out of file descriptors,
    /// which is exactly the condition a helper storm creates. An
    /// earlier revision joined the acceptor after the dial, so a failed
    /// dial parked the helper forever: it never released its inotify
    /// instance and never removed its endpoint file, and clients that
    /// later read that file got ECONNREFUSED.
    ///
    /// This pins the observable half of that contract — the loop
    /// returns without a connection ever arriving. The dial-fails case
    /// is excluded structurally rather than simulated: there is no
    /// `join`, so no code path can wait on the acceptor at all.
    #[test]
    fn the_exit_path_does_not_wait_on_the_acceptor_thread() {
        let listener = TcpListener::bind((HELPER_HOST, 0)).unwrap();

        let started = Instant::now();
        run_server_loop(&listener, &mut ExitAtOnce).unwrap();
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(5),
            "exiting an idle loop took {elapsed:?}; the teardown is blocking on the acceptor"
        );
    }
}
