// SPDX-License-Identifier: Apache-2.0
//! `heddle daemon …` CLI dispatchers.
//!
//! Three verbs:
//!
//! * `serve` — runs the foreground daemon. Linux + `--features mount`
//!   only; everywhere else it returns the standard
//!   `virtualized_unsupported_error`.
//! * `status` — sends `health` to a running daemon and prints the
//!   reply. No-op success when the daemon isn't running, so
//!   operators can run `heddle daemon status` as a probe.
//! * `stop` — sends `shutdown`, waits for the endpoint file to
//!   disappear *and* the daemon PID to die, then sweeps any
//!   leftover mounts as a safety net. The combined wait gives
//!   callers a hard post-condition (see `cmd_daemon_stop`).

use std::time::Duration;

use anyhow::{Result, anyhow};
use repo::daemon::{
    MountDaemonRequest, MountDaemonResponse, load_endpoint, mount_daemon_endpoint_path, pid_alive,
};
use serde::Serialize;

use super::client::{rpc, sweep_stale_mounts};
use crate::cli::{
    Cli, commands::advice::RecoveryAdvice, render::write_json_stdout, should_output_json,
};

#[derive(Debug, Serialize)]
struct DaemonStopOutput {
    output_kind: &'static str,
    action: &'static str,
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct DaemonStatusOutput {
    status: &'static str,
    running: bool,
    endpoint_path: String,
    ok: bool,
    version: Option<u32>,
    uptime_s: Option<u64>,
    mount_count: usize,
    materialized_count: usize,
    materialized_threads: Vec<MaterializedThreadStatus>,
}

#[derive(Debug, Serialize)]
struct MaterializedThreadStatus {
    thread: String,
    state: String,
    files: usize,
    tree: String,
}

#[cfg(all(target_os = "linux", feature = "mount"))]
pub fn cmd_daemon_serve(cli: &Cli) -> Result<()> {
    let repo_root = resolve_repo_root(cli)?;
    super::server::run_mount_daemon(&repo_root)
}

#[cfg(not(all(target_os = "linux", feature = "mount")))]
pub fn cmd_daemon_serve(_cli: &Cli) -> Result<()> {
    Err(
        crate::cli::commands::mount_lifecycle::virtualized_unsupported_error()
            .context("heddle daemon serve"),
    )
}

pub fn cmd_daemon_status(cli: &Cli) -> Result<()> {
    let repo_root = resolve_repo_root(cli)?;
    let response = rpc(&repo_root, &MountDaemonRequest::Health {}, false)?;
    // Materialized threads are persistent on disk (clonefile-backed),
    // not held in the daemon's live mount registry. Enumerate them
    // from the on-disk manifests so `daemon status` surfaces the
    // full picture: virtualised mounts (daemon-resident) + materialised
    // threads (daemon-independent). Best-effort — a malformed
    // manifest directory shouldn't break the status output.
    //
    // Use `repo.heddle_dir()` rather than `repo_root.join(".heddle")`
    // because in a worktree those aren't the same path: the
    // worktree's `.heddle/objectstore` pointer forwards to the main
    // repo's heddle dir (set up by `Repository::open` via
    // `with_local_head`), and the manifests live in the *main*
    // repo's `threads/`. Pre-fix this misread always returned an
    // empty inventory inside a worktree.
    let heddle_dir = resolve_heddle_dir(cli).unwrap_or_else(|_| repo_root.join(".heddle"));
    let materialized =
        repo::thread_manifest::list_thread_manifests(&heddle_dir).unwrap_or_default();
    let materialized_threads = materialized
        .iter()
        .map(|manifest| MaterializedThreadStatus {
            thread: manifest.thread.clone(),
            state: manifest.state_id.to_string(),
            files: manifest.file_count,
            tree: manifest.tree_hash.to_string()[..12].to_string(),
        })
        .collect::<Vec<_>>();
    let endpoint_path = mount_daemon_endpoint_path(&repo_root).display().to_string();
    let json = should_output_json(cli, None);
    match response {
        Some(MountDaemonResponse::Health {
            version,
            ok,
            uptime_s,
            mount_count,
        }) => {
            if json {
                let output = DaemonStatusOutput {
                    status: "running",
                    running: true,
                    endpoint_path,
                    ok,
                    version: Some(version),
                    uptime_s: Some(uptime_s),
                    mount_count,
                    materialized_count: materialized_threads.len(),
                    materialized_threads,
                };
                crate::cli::render::write_json_stdout(&output)?;
                return Ok(());
            } else {
                println!(
                    "daemon: ok={ok} version={version} uptime_s={uptime_s} mount_count={mount_count} materialized_count={}",
                    materialized.len()
                );
            }
        }
        Some(MountDaemonResponse::Error { code, message, .. }) => {
            return Err(anyhow!(daemon_response_refusal(
                "daemon_health_failed",
                format!("daemon health failed: [{code}] {message}"),
                format!("daemon returned error code {code}: {message}"),
                "heddle daemon status",
            )));
        }
        Some(other) => {
            return Err(anyhow!(daemon_response_refusal(
                "daemon_unexpected_response",
                format!("unexpected daemon response: {other:?}"),
                format!(
                    "daemon returned a response variant that `status` cannot interpret: {other:?}"
                ),
                "heddle daemon status",
            )));
        }
        None => {
            if json {
                let output = DaemonStatusOutput {
                    status: "not_running",
                    running: false,
                    endpoint_path,
                    ok: false,
                    version: None,
                    uptime_s: None,
                    mount_count: 0,
                    materialized_count: materialized_threads.len(),
                    materialized_threads,
                };
                crate::cli::render::write_json_stdout(&output)?;
                return Ok(());
            } else {
                println!(
                    "daemon: not running (no live endpoint at {}) materialized_count={}",
                    mount_daemon_endpoint_path(&repo_root).display(),
                    materialized.len()
                );
            }
        }
    }
    if !materialized.is_empty() {
        println!("materialized threads:");
        for s in &materialized {
            println!(
                "  {} (state={}, files={}, tree={})",
                s.thread,
                s.state_id,
                s.file_count,
                &s.tree_hash.to_string()[..12]
            );
        }
    }
    Ok(())
}

/// Post-condition contract for `cmd_daemon_stop`: when this returns
/// `Ok(())` after a live-daemon shutdown, the caller may rely on
/// **all four** of the following being true:
///
/// 1. The daemon process (whose PID was advertised in the endpoint
///    file) has exited (`kill -0` returns `ESRCH`).
/// 2. `<repo>/.heddle/state/heddled.endpoint.json` no longer exists.
/// 3. `<repo>/.heddle/state/mounts.json` no longer exists. The
///    daemon's `MountRegistry::shutdown_all` removes it before
///    `remove_endpoint`, and the CLI-side `sweep_stale_mounts` runs
///    as a safety-net (idempotent — both use `fs::remove_file` and
///    swallow `NotFound`).
/// 4. Any FUSE mountpoints the daemon owned are unmounted (best-effort
///    via the `BackgroundSession` drop in `LiveMount::shutdown`, with
///    `fusermount -u` as a fallback inside `sweep_stale_mounts`).
///
/// Two timeouts are layered to make the contract observable rather
/// than hopeful: 2 s for the endpoint file to disappear (proof the
/// daemon's `run_mount_daemon` reached its post-shutdown cleanup), and
/// a further 2 s for the PID to be reaped. Either can elapse without
/// failing the call — the safety-net sweep still runs — but together
/// they make the integration-test assertions deterministic.
pub fn cmd_daemon_stop(cli: &Cli) -> Result<()> {
    let repo_root = resolve_repo_root(cli)?;
    let json = should_output_json(cli, None);
    let endpoint_path = mount_daemon_endpoint_path(&repo_root);
    // Capture the daemon PID *before* sending shutdown so we can
    // probe it via `kill -0` after the endpoint file is gone. If the
    // endpoint file has no recorded PID (v1-era files, or a future
    // schema change) we just skip the PID wait — the endpoint-gone
    // observation is still load-bearing.
    let recorded_pid = load_endpoint(&endpoint_path).ok().and_then(|e| e.pid);
    match rpc(&repo_root, &MountDaemonRequest::Shutdown {}, false)? {
        Some(MountDaemonResponse::Shutdown { ok: true, .. }) => {}
        Some(MountDaemonResponse::Error { code, message, .. }) => {
            return Err(anyhow!(daemon_response_refusal(
                "daemon_shutdown_refused",
                format!("daemon refused shutdown: [{code}] {message}"),
                format!("daemon returned error code {code}: {message}"),
                "heddle daemon status",
            )));
        }
        Some(other) => {
            return Err(anyhow!(daemon_response_refusal(
                "daemon_unexpected_response",
                format!("unexpected daemon response: {other:?}"),
                format!(
                    "daemon returned a response variant that `stop` cannot interpret: {other:?}"
                ),
                "heddle daemon status",
            )));
        }
        None => {
            if json {
                write_json_stdout(&DaemonStopOutput {
                    output_kind: "daemon_stop",
                    action: "daemon stop",
                    status: "not_running",
                })?;
            } else {
                println!("daemon: not running");
            }
            return Ok(());
        }
    }
    // Phase 1: wait up to 2 s for the endpoint file to disappear.
    // The daemon's `run_mount_daemon` removes it *after*
    // `MountRegistry::shutdown_all` (which removes `mounts.json`),
    // so endpoint-gone implies mounts.json-gone on the daemon side.
    for _ in 0..40 {
        if !endpoint_path.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // Phase 2: wait up to a further 2 s for the daemon process
    // itself to exit. Without this, the endpoint-gone observation
    // races the daemon's final `info!("heddle daemon exiting")` +
    // process teardown — a caller probing PID liveness right after
    // `daemon stop` returns could still see the PID briefly. Polling
    // here turns the post-condition from "shutdown is in flight"
    // into "shutdown is complete".
    if let Some(pid) = recorded_pid {
        for _ in 0..40 {
            if !pid_alive(pid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    // Sweep any leftover registry entries as a last-resort safety
    // net for crash-during-shutdown scenarios. Idempotent: in the
    // happy path the daemon has already removed `mounts.json`, so
    // this is a no-op.
    sweep_stale_mounts(&repo_root);
    if json {
        write_json_stdout(&DaemonStopOutput {
            output_kind: "daemon_stop",
            action: "daemon stop",
            status: "stopped",
        })?;
    } else {
        println!("daemon: stopped");
    }
    Ok(())
}

fn daemon_response_refusal(
    kind: &'static str,
    error: impl Into<String>,
    unsafe_condition: impl Into<String>,
    primary_command: impl Into<String>,
) -> RecoveryAdvice {
    let primary_command = primary_command.into();
    RecoveryAdvice::safety_refusal(
        kind,
        error,
        format!("Inspect the daemon with `{primary_command}` before retrying."),
        unsafe_condition,
        "continuing could accept stale mount-daemon state or act on the wrong daemon response",
        "repository objects, refs, worktree files, and mount registry files were left unchanged",
        primary_command.clone(),
        vec![primary_command],
    )
}

fn resolve_repo_root(cli: &Cli) -> Result<std::path::PathBuf> {
    if let Some(root) = cli.repo.as_ref() {
        return Ok(root.clone());
    }
    let repo = repo::Repository::open(&std::env::current_dir()?)?;
    Ok(repo.root().to_path_buf())
}

/// Resolve the heddle dir for the currently-open repo. Differs from
/// `<repo_root>/.heddle` for worktrees: those have a `.heddle/`
/// pointer file forwarding to the main repo's heddle dir, and
/// thread manifests live in the main repo's `threads/`. Always
/// opens the repo to read the canonical heddle_dir from there.
fn resolve_heddle_dir(cli: &Cli) -> Result<std::path::PathBuf> {
    let start = cli
        .repo
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));
    let repo = repo::Repository::open(&start)?;
    Ok(repo.heddle_dir().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::daemon_response_refusal;

    #[test]
    fn daemon_response_refusal_carries_typed_recovery_fields() {
        let advice = daemon_response_refusal(
            "daemon_health_failed",
            "daemon health failed: [boom] nope",
            "daemon returned error code boom: nope",
            "heddle daemon status",
        );
        assert_eq!(advice.kind, "daemon_health_failed");
        assert_eq!(advice.primary_command, "heddle daemon status");
        assert!(advice.hint.contains("heddle daemon status"));
        assert!(advice.would_change.contains("stale mount-daemon state"));
    }
}
