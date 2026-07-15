// SPDX-License-Identifier: Apache-2.0
//! Pure mount-daemon status/stop message assembly (no endpoint I/O).
//!
//! Owns human lines and status tokens for `heddle daemon status|stop` that
//! can be decided from RPC/list facts alone. Endpoint load, RPC, PID wait,
//! and RecoveryAdvice stay CLI-owned.

/// JSON/status token when the daemon answered Health.
pub fn daemon_running_status_token() -> &'static str {
    "running"
}

/// JSON/status token when no live endpoint answered.
pub fn daemon_not_running_status_token() -> &'static str {
    "not_running"
}

/// JSON status after a successful stop.
pub fn daemon_stopped_status_token() -> &'static str {
    "stopped"
}

/// Human line when status finds a live daemon.
pub fn daemon_status_running_line(
    ok: bool,
    version: u32,
    uptime_s: u64,
    mount_count: usize,
    materialized_count: usize,
) -> String {
    format!(
        "daemon: ok={ok} version={version} uptime_s={uptime_s} mount_count={mount_count} materialized_count={materialized_count}"
    )
}

/// Human line when status finds no live endpoint.
pub fn daemon_status_not_running_line(endpoint_path: &str, materialized_count: usize) -> String {
    format!(
        "daemon: not running (no live endpoint at {endpoint_path}) materialized_count={materialized_count}"
    )
}

/// Human line after stop when the daemon was not running.
pub fn daemon_stop_not_running_line() -> &'static str {
    "daemon: not running"
}

/// Human line after a successful stop.
pub fn daemon_stop_stopped_line() -> &'static str {
    "daemon: stopped"
}

/// Header for the materialized-thread inventory section.
pub fn daemon_materialized_threads_header() -> &'static str {
    "materialized threads:"
}

/// One indented materialized-thread inventory line.
pub fn daemon_materialized_thread_line(
    thread: &str,
    state: &str,
    files: usize,
    tree_short: &str,
) -> String {
    format!("  {thread} (state={state}, files={files}, tree={tree_short})")
}

/// First 12 characters of a tree / object hash string for status display.
pub fn daemon_short_tree(tree: &str) -> &str {
    &tree[..tree.len().min(12)]
}

/// Stable recovery-advice kind tokens for daemon response failures.
pub fn daemon_health_failed_kind() -> &'static str {
    "daemon_health_failed"
}

/// Kind when health/stop received an unexpected response variant.
pub fn daemon_unexpected_response_kind() -> &'static str {
    "daemon_unexpected_response"
}

/// Kind when shutdown was refused by the daemon.
pub fn daemon_shutdown_refused_kind() -> &'static str {
    "daemon_shutdown_refused"
}

/// Status token for stop JSON from whether a live daemon was contacted.
pub fn daemon_stop_status_token(was_running: bool) -> &'static str {
    if was_running {
        daemon_stopped_status_token()
    } else {
        daemon_not_running_status_token()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_tokens_and_lines() {
        assert_eq!(daemon_running_status_token(), "running");
        assert_eq!(daemon_not_running_status_token(), "not_running");
        assert_eq!(daemon_stopped_status_token(), "stopped");
        assert_eq!(daemon_stop_status_token(true), "stopped");
        assert_eq!(daemon_stop_status_token(false), "not_running");

        let running = daemon_status_running_line(true, 1, 42, 2, 3);
        assert!(running.contains("ok=true"));
        assert!(running.contains("uptime_s=42"));
        assert!(running.contains("materialized_count=3"));

        let idle = daemon_status_not_running_line("/tmp/ep.json", 1);
        assert!(idle.contains("not running"));
        assert!(idle.contains("/tmp/ep.json"));
        assert_eq!(daemon_stop_not_running_line(), "daemon: not running");
        assert_eq!(daemon_stop_stopped_line(), "daemon: stopped");
    }

    #[test]
    fn materialized_line_and_kinds() {
        assert_eq!(
            daemon_materialized_thread_line("feat", "abc", 10, "deadbeefcafe"),
            "  feat (state=abc, files=10, tree=deadbeefcafe)"
        );
        assert_eq!(daemon_short_tree("0123456789abcdef"), "0123456789ab");
        assert_eq!(daemon_health_failed_kind(), "daemon_health_failed");
        assert_eq!(
            daemon_unexpected_response_kind(),
            "daemon_unexpected_response"
        );
        assert_eq!(daemon_shutdown_refused_kind(), "daemon_shutdown_refused");
        assert_eq!(
            daemon_materialized_threads_header(),
            "materialized threads:"
        );
    }
}
