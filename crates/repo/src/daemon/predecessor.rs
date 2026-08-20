// SPDX-License-Identifier: Apache-2.0
//! Retire a live older mount daemon before spawning a replacement.
//!
//! A v2 TCP daemon is still a process that owns FUSE mounts. Classifying
//! its endpoint as stale and respawning without `Shutdown` leaves two
//! daemons. Fail closed if the predecessor does not exit.

use std::{
    net::TcpStream,
    time::{Duration, Instant},
};

use objects::error::HeddleError;

use super::{
    endpoint::{EndpointState, pid_alive},
    mount_proto::{
        MountDaemonRequest, MountDaemonResponse, MountEndpointDisposition,
        mount_endpoint_disposition,
    },
    protocol::send_json_request,
};

const RETIRE_POLL_MS: u64 = 50;
const RETIRE_WAIT: Duration = Duration::from_secs(2);

/// Stop a live v2 TCP predecessor. Current and already-stale endpoints
/// are no-ops. If the process is still alive after the wait, return
/// `Err` so the caller does not spawn a second daemon.
pub fn retire_live_tcp_predecessor(endpoint: &EndpointState) -> Result<(), HeddleError> {
    let pid_is_alive = endpoint.pid.is_some_and(pid_alive);
    match mount_endpoint_disposition(endpoint, pid_is_alive) {
        MountEndpointDisposition::Current | MountEndpointDisposition::Stale => Ok(()),
        MountEndpointDisposition::LiveTcpPredecessor => stop_tcp_predecessor(endpoint),
    }
}

fn stop_tcp_predecessor(endpoint: &EndpointState) -> Result<(), HeddleError> {
    match send_json_request::<_, MountDaemonResponse>(endpoint, &MountDaemonRequest::Shutdown {}) {
        Ok(MountDaemonResponse::Shutdown { .. }) => {}
        Ok(other) => {
            return Err(HeddleError::Config(format!(
                "live v{} mount daemon refused shutdown: {other:?}",
                endpoint.version
            )));
        }
        Err(_) => {
            // Connection refused means the process already exited; still
            // wait so we do not race a dying PID.
        }
    }
    if !wait_until_predecessor_gone(endpoint) {
        return Err(HeddleError::Config(format!(
            "live v{} mount daemon did not exit after shutdown; refusing to spawn a second daemon",
            endpoint.version
        )));
    }
    Ok(())
}

fn wait_until_predecessor_gone(endpoint: &EndpointState) -> bool {
    let deadline = Instant::now() + RETIRE_WAIT;
    loop {
        if predecessor_is_gone(endpoint) {
            return true;
        }
        if Instant::now() >= deadline {
            return predecessor_is_gone(endpoint);
        }
        std::thread::sleep(Duration::from_millis(RETIRE_POLL_MS));
    }
}

fn predecessor_is_gone(endpoint: &EndpointState) -> bool {
    if let Some(pid) = endpoint.pid
        && pid_alive(pid)
    {
        return false;
    }
    !tcp_endpoint_accepts(endpoint)
}

fn tcp_endpoint_accepts(endpoint: &EndpointState) -> bool {
    let address = format!("{}:{}", endpoint.host, endpoint.port);
    let Ok(addr) = address.parse() else {
        return false;
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok()
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        process::{Command, Stdio},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
    };

    use super::{mount_endpoint_disposition, retire_live_tcp_predecessor};
    use crate::daemon::{
        EndpointState, MOUNT_PROTOCOL_V2, MOUNT_PROTOCOL_VERSION, MountDaemonResponse,
        MountEndpointDisposition, pid_alive,
    };

    fn v2_tcp(host: &str, port: u16, pid: u32) -> EndpointState {
        EndpointState {
            version: MOUNT_PROTOCOL_V2,
            host: host.to_string(),
            port,
            pid: Some(pid),
            socket_path: None,
        }
    }

    #[test]
    fn live_v2_tcp_is_a_predecessor() {
        let endpoint = v2_tcp("127.0.0.1", 9, 1);
        assert_eq!(
            mount_endpoint_disposition(&endpoint, true),
            MountEndpointDisposition::LiveTcpPredecessor
        );
    }

    #[test]
    fn dead_v2_tcp_is_stale() {
        let endpoint = v2_tcp("127.0.0.1", 9, 0x7fff_fffe);
        assert_eq!(
            mount_endpoint_disposition(&endpoint, false),
            MountEndpointDisposition::Stale
        );
    }

    #[test]
    fn current_uds_is_not_a_predecessor() {
        let endpoint = EndpointState {
            version: MOUNT_PROTOCOL_VERSION,
            host: "unix".to_string(),
            port: 0,
            pid: Some(1),
            socket_path: Some("/tmp/heddled.sock".into()),
        };
        assert_eq!(
            mount_endpoint_disposition(&endpoint, true),
            MountEndpointDisposition::Current
        );
    }

    fn spawn_sleep() -> std::process::Child {
        Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep as stand-in daemon pid")
    }

    fn serve_v2_shutdown(
        listener: TcpListener,
        shutdown: Arc<AtomicBool>,
        mut child: std::process::Child,
    ) {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut line = String::new();
            let _ = BufReader::new(stream.try_clone().expect("clone")).read_line(&mut line);
            if line.contains("shutdown") {
                shutdown.store(true, Ordering::Release);
                let reply = MountDaemonResponse::Shutdown {
                    version: MOUNT_PROTOCOL_V2,
                    ok: true,
                };
                let _ = serde_json::to_writer(&mut stream, &reply);
                let _ = stream.write_all(b"\n");
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn upgrade_retires_live_v2_before_returning() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind v2 stand-in");
        let port = listener.local_addr().expect("port").port();
        let child = spawn_sleep();
        let pid = child.id();
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&shutdown);
        let server = thread::spawn(move || serve_v2_shutdown(listener, flag, child));

        let endpoint = v2_tcp("127.0.0.1", port, pid);
        retire_live_tcp_predecessor(&endpoint).expect("live v2 must retire");
        assert!(
            shutdown.load(Ordering::Acquire),
            "v2 Shutdown must be sent before the caller may respawn"
        );
        assert!(
            !pid_alive(pid),
            "retired v2 pid must be gone before respawn"
        );
        let _ = server.join();
    }

    #[test]
    fn upgrade_fails_closed_when_live_v2_stays_alive() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stubborn v2");
        let port = listener.local_addr().expect("port").port();
        let mut child = spawn_sleep();
        let pid = child.id();
        let server = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut line = String::new();
                let _ = BufReader::new(&stream).read_line(&mut line);
                let reply = MountDaemonResponse::Shutdown {
                    version: MOUNT_PROTOCOL_V2,
                    ok: true,
                };
                let _ = serde_json::to_writer(&mut stream, &reply);
                let _ = stream.write_all(b"\n");
                // Leave `child` alive on purpose.
            }
        });

        let endpoint = v2_tcp("127.0.0.1", port, pid);
        let error = retire_live_tcp_predecessor(&endpoint)
            .expect_err("a still-alive v2 daemon must block respawn");
        assert!(
            error
                .to_string()
                .contains("refusing to spawn a second daemon"),
            "got {error}"
        );
        let _ = child.kill();
        let _ = child.wait();
        let _ = server.join();
    }
}
