// SPDX-License-Identifier: Apache-2.0
//! `heddle netd …` CLI dispatchers.
//!
//! Box-scoped control surface for the network daemon, parallel to the
//! mount daemon's `serve`/`status`/`stop` but anchored at
//! `<heddle_home>` and not gated on Linux/`--features mount`:
//!
//! * `serve` — run the foreground network daemon (async). Requires a
//!   Unix host and the hosted `client` surface; elsewhere it returns
//!   an unsupported error.
//! * `status` — probe the running daemon over its same-uid socket and
//!   print liveness + advertised node id. No-op success when the
//!   daemon isn't running.
//! * `stop` — ask the running daemon to close its endpoint and exit,
//!   then wait for the discovery file to disappear and the PID to die.

#[cfg(unix)]
use std::time::Duration;

use anyhow::Result;
use serde::Serialize;

use super::super::next_action::{NextActionValidationContext, write_full_command_json};
use crate::cli::{Cli, should_output_json};
#[cfg(unix)]
use crate::cli::commands::netdaemon::proto::{
    NETWORK_DAEMON_PROTOCOL_VERSION, NetworkDaemonRequest, NetworkDaemonResponse,
    network_daemon_endpoint_path,
};

#[derive(Debug, Serialize)]
struct NetdStatusOutput {
    output_kind: &'static str,
    status: &'static str,
    running: bool,
    endpoint_path: String,
    node_id: Option<String>,
    version: Option<u32>,
    uptime_s: Option<u64>,
}

#[derive(Debug, Serialize)]
struct NetdStopOutput {
    output_kind: &'static str,
    action: &'static str,
    status: &'static str,
}

#[cfg(all(unix, feature = "client"))]
pub async fn cmd_netd_serve(_cli: &Cli) -> Result<()> {
    super::server::run_network_daemon().await
}

#[cfg(not(all(unix, feature = "client")))]
pub async fn cmd_netd_serve(_cli: &Cli) -> Result<()> {
    Err(netd_unsupported_error())
}

#[cfg(unix)]
pub fn cmd_netd_status(cli: &Cli) -> Result<()> {
    use repo::daemon::send_json_request_unix;

    let heddle_home = repo::identity::heddle_home_dir();
    let endpoint_path = network_daemon_endpoint_path(&heddle_home);
    let endpoint_display = endpoint_path.display().to_string();
    let json = should_output_json(cli, None);

    // A live daemon is one whose discovery file matches our protocol
    // version and whose recorded PID is still alive. Enrich with a
    // Health RPC over the same-uid socket when we can reach it.
    let live = read_live_netd(&endpoint_path);
    let (running, node_id, version, uptime_s) = match live {
        Some(endpoint) => {
            let health = endpoint
                .socket_path
                .as_deref()
                .and_then(|socket| {
                    send_json_request_unix::<_, NetworkDaemonResponse>(
                        socket,
                        &NetworkDaemonRequest::Health {},
                    )
                    .ok()
                })
                .and_then(|response| match response {
                    NetworkDaemonResponse::Health {
                        node_id, uptime_s, ..
                    } => Some((node_id, uptime_s)),
                    _ => None,
                });
            let node_id = health
                .as_ref()
                .map(|(node_id, _)| node_id.clone())
                .or_else(|| endpoint.node_id.clone());
            let uptime_s = health.map(|(_, uptime_s)| uptime_s);
            (true, node_id, Some(endpoint.version), uptime_s)
        }
        None => (false, None, None, None),
    };

    if json {
        let output = NetdStatusOutput {
            output_kind: "netd_status",
            status: if running { "running" } else { "not_running" },
            running,
            endpoint_path: endpoint_display,
            node_id,
            version,
            uptime_s,
        };
        return write_full_command_json(
            &output,
            NextActionValidationContext::without_repo(&["netd", "status"]),
        );
    }

    if running {
        let node = node_id.as_deref().unwrap_or("<unknown>");
        match uptime_s {
            Some(uptime) => {
                println!("network daemon running (node {node}, uptime {uptime}s)")
            }
            None => println!("network daemon running (node {node})"),
        }
    } else {
        println!("network daemon not running ({endpoint_display})");
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn cmd_netd_status(_cli: &Cli) -> Result<()> {
    Err(netd_unsupported_error())
}

#[cfg(unix)]
pub fn cmd_netd_stop(cli: &Cli) -> Result<()> {
    use repo::daemon::{load_endpoint, pid_alive, send_json_request_unix};

    let heddle_home = repo::identity::heddle_home_dir();
    let endpoint_path = network_daemon_endpoint_path(&heddle_home);
    let json = should_output_json(cli, None);

    let existing = load_endpoint(&endpoint_path).ok();
    let recorded_pid = existing.as_ref().and_then(|endpoint| endpoint.pid);
    let socket_path = existing.as_ref().and_then(|endpoint| endpoint.socket_path.clone());

    let daemon_running = recorded_pid.map(pid_alive).unwrap_or(false);
    if !daemon_running {
        return report_stop(json, false);
    }

    if let Some(socket) = socket_path.as_deref() {
        // Best-effort: the daemon acks then drains. A transport error
        // here (daemon exited between our liveness check and the
        // connect) is not fatal — we still wait for the file to clear.
        let _ = send_json_request_unix::<_, NetworkDaemonResponse>(
            socket,
            &NetworkDaemonRequest::Shutdown {},
        );
    }

    // Wait up to 2s for the discovery file to disappear (proof the
    // daemon reached its post-shutdown cleanup).
    for _ in 0..40 {
        if !endpoint_path.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // Then up to a further 2s for the PID to be reaped, so the
    // post-condition is "stopped", not "stopping".
    if let Some(pid) = recorded_pid {
        for _ in 0..40 {
            if !pid_alive(pid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    report_stop(json, true)
}

#[cfg(not(unix))]
pub fn cmd_netd_stop(_cli: &Cli) -> Result<()> {
    Err(netd_unsupported_error())
}

fn report_stop(json: bool, stopped: bool) -> Result<()> {
    if json {
        return write_full_command_json(
            &NetdStopOutput {
                output_kind: "netd_stop",
                action: "netd stop",
                status: if stopped { "stopped" } else { "not_running" },
            },
            NextActionValidationContext::without_repo(&["netd", "stop"]),
        );
    }
    if stopped {
        println!("network daemon stopped");
    } else {
        println!("network daemon not running");
    }
    Ok(())
}

/// Return the running daemon's discovery record, or `None` when the
/// file is missing, version-skewed, or points at a dead PID.
#[cfg(unix)]
fn read_live_netd(endpoint_path: &std::path::Path) -> Option<repo::daemon::EndpointState> {
    use repo::daemon::{load_endpoint, pid_alive};

    let endpoint = load_endpoint(endpoint_path).ok()?;
    if endpoint.version != NETWORK_DAEMON_PROTOCOL_VERSION {
        return None;
    }
    match endpoint.pid {
        Some(pid) if pid_alive(pid) => Some(endpoint),
        _ => None,
    }
}

#[cfg(not(all(unix, feature = "client")))]
fn netd_unsupported_error() -> anyhow::Error {
    anyhow::anyhow!(
        "the heddle network daemon requires a Unix host built with the `client` feature"
    )
}
