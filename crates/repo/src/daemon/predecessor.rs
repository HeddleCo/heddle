// SPDX-License-Identifier: Apache-2.0
//! Refuse to spawn over a live older TCP mount daemon.
//!
//! A leftover v2 process still owns FUSE mounts. v3 does not speak
//! that protocol. Fail closed and tell the operator to stop or
//! upgrade the leftover process.

use objects::error::HeddleError;

use super::{
    endpoint::{EndpointState, pid_alive},
    mount_proto::MOUNT_PROTOCOL_VERSION,
};

/// Fail closed when a live older TCP daemon still owns this repo.
/// Current and already-stale endpoints are no-ops.
pub fn refuse_live_legacy_tcp_endpoint(endpoint: &EndpointState) -> Result<(), HeddleError> {
    if !is_live_legacy_tcp(endpoint) {
        return Ok(());
    }
    let pid = endpoint
        .pid
        .map(|pid| pid.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    Err(HeddleError::Config(format!(
        "live v{} TCP mount daemon (pid {pid} at {}:{}) still owns this repo; \
         refusing to spawn a replacement. Stop or upgrade the leftover process, then retry",
        endpoint.version, endpoint.host, endpoint.port
    )))
}

fn is_live_legacy_tcp(endpoint: &EndpointState) -> bool {
    endpoint.version < MOUNT_PROTOCOL_VERSION
        && endpoint.socket_path.is_none()
        && endpoint.pid.is_some_and(pid_alive)
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};

    use super::refuse_live_legacy_tcp_endpoint;
    use crate::daemon::{EndpointState, MOUNT_PROTOCOL_V2, MOUNT_PROTOCOL_VERSION, pid_alive};

    fn v2_tcp(host: &str, port: u16, pid: u32) -> EndpointState {
        EndpointState {
            version: MOUNT_PROTOCOL_V2,
            host: host.to_string(),
            port,
            pid: Some(pid),
            socket_path: None,
        }
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

    #[test]
    fn live_v2_tcp_fails_closed_with_recovery() {
        let mut child = spawn_sleep();
        let pid = child.id();
        assert!(pid_alive(pid), "stand-in pid must be live");
        let error = refuse_live_legacy_tcp_endpoint(&v2_tcp("127.0.0.1", 9, pid))
            .expect_err("live v2 must fail closed");
        let message = error.to_string();
        assert!(
            message.contains("refusing to spawn a replacement"),
            "got {error}"
        );
        assert!(
            message.contains("Stop or upgrade the leftover process"),
            "got {error}"
        );
        assert!(message.contains(&format!("pid {pid}")), "got {error}");
        assert!(message.contains("v2"), "got {error}");
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn dead_v2_tcp_is_not_a_live_predecessor() {
        refuse_live_legacy_tcp_endpoint(&v2_tcp("127.0.0.1", 9, 0x7fff_fffe))
            .expect("dead v2 must not block spawn");
    }

    #[test]
    fn current_uds_is_not_a_legacy_tcp_endpoint() {
        let endpoint = EndpointState {
            version: MOUNT_PROTOCOL_VERSION,
            host: "unix".to_string(),
            port: 0,
            pid: Some(1),
            socket_path: Some("/tmp/heddled.sock".into()),
        };
        refuse_live_legacy_tcp_endpoint(&endpoint).expect("current UDS must be usable");
    }
}
