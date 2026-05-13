// SPDX-License-Identifier: Apache-2.0
//! `heddle agent serve|status|stop` handlers.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use repo::Repository;
use serde::Serialize;
#[cfg(unix)]
use daemon::local_daemon::{
    LocalDaemonConfig, PidFileContents, default_pid_path, default_socket_path, is_heddle_process,
    serve,
};

use crate::cli::{
    cli_args::{AgentCommands, AgentServeArgs, Cli},
    should_output_json,
};

#[derive(Serialize)]
struct AgentStatusOutput {
    running: bool,
    pid: Option<u32>,
    socket_path: String,
    pid_path: String,
}

pub async fn run(cli: &Cli, command: &AgentCommands) -> Result<()> {
    match command {
        AgentCommands::Serve(args) => run_serve(cli, args).await,
        AgentCommands::Status => run_status(cli).await,
        AgentCommands::Stop => run_stop(cli).await,
        // Reservation-API variants delegate to the cmd_agent_* fns
        // in agent_cmd.rs. We dispatch here so main.rs has a single
        // entry point per top-level command.
        AgentCommands::Reserve(args) => super::agent_cmd::cmd_agent_reserve(cli, args.clone()),
        AgentCommands::Heartbeat(args) => super::agent_cmd::cmd_agent_heartbeat(cli, args.clone()),
        AgentCommands::Capture(args) => {
            super::agent_cmd::cmd_agent_capture(cli, args.clone()).await
        }
        AgentCommands::Ready(args) => super::agent_cmd::cmd_agent_ready(cli, args.clone()).await,
        AgentCommands::Release(args) => super::agent_cmd::cmd_agent_release(cli, args.clone()),
        AgentCommands::List(args) => super::agent_cmd::cmd_agent_list(cli, args.clone()),
    }
}

async fn run_serve(cli: &Cli, args: &AgentServeArgs) -> Result<()> {
    #[cfg(not(unix))]
    {
        let _ = (cli, args);
        return Err(anyhow!(
            "heddle agent serve is only supported on Unix platforms"
        ));
    }
    #[cfg(unix)]
    {
        let repo = open_repo()?;
        let mut config = LocalDaemonConfig::from_repo(&repo);
        if let Some(socket) = args.socket.clone() {
            config = config.with_socket(socket);
        }
        if !args.foreground {
            // First-ship simplification: foreground only. Daemonization on
            // Unix needs careful fork+setsid handling and is best layered
            // with a battle-tested helper. The CLI tip nudges users toward
            // `heddle agent serve --foreground &`.
            return Err(anyhow!(
                "background daemonization is not yet implemented; pass --foreground"
            ));
        }
        if !should_output_json(cli, Some(repo.config())) {
            eprintln!(
                "heddle agent serve: listening on {}",
                config.socket_path.display()
            );
            eprintln!(
                "heddle agent serve: pidfile at {}",
                config.pid_path.display()
            );
        }
        let shutdown = async {
            let _ = tokio::signal::ctrl_c().await;
        };
        serve(repo, config, shutdown)
            .await
            .map_err(|e| anyhow!("local daemon failed: {e}"))?;
        Ok(())
    }
}

async fn run_status(cli: &Cli) -> Result<()> {
    let repo = open_repo()?;
    let pid_path = pid_path(&repo);
    let socket_path = socket_path(&repo);
    let pid = read_pid(&pid_path);
    let running = pid.map(pid_alive).unwrap_or(false);
    let output = AgentStatusOutput {
        running,
        pid,
        socket_path: socket_path.display().to_string(),
        pid_path: pid_path.display().to_string(),
    };
    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{}",
            serde_json::to_string(&output).context("serialize agent status")?
        );
    } else if running {
        println!(
            "heddle agent: running (pid {})\n  socket: {}\n  pidfile: {}",
            output.pid.unwrap_or(0),
            output.socket_path,
            output.pid_path
        );
    } else {
        println!("heddle agent: not running");
        println!("  socket: {}", output.socket_path);
        println!("  pidfile: {}", output.pid_path);
    }
    Ok(())
}

async fn run_stop(cli: &Cli) -> Result<()> {
    let repo = open_repo()?;
    let pid_path = pid_path(&repo);

    // Read the pidfile and require the heddle marker. A pidfile lacking
    // our marker shape is treated as "not ours" — refuse to act rather
    // than risk SIGTERMing whatever PID happens to be in there.
    let raw = match std::fs::read_to_string(&pid_path) {
        Ok(s) => s,
        Err(_) => {
            if !should_output_json(cli, Some(repo.config())) {
                println!("heddle agent: not running (no pidfile)");
            } else {
                println!("{{\"stopped\":false,\"reason\":\"no pidfile\"}}");
            }
            return Ok(());
        }
    };
    #[cfg(unix)]
    {
        let parsed = match PidFileContents::parse(&raw) {
            Some(c) => c,
            None => {
                return Err(anyhow!(
                    "pidfile at {} is not in the heddle agent format; refusing to send a signal",
                    pid_path.display()
                ));
            }
        };
        let pid = parsed.pid;
        if !pid_alive(pid as u32) {
            let _ = std::fs::remove_file(&pid_path);
            if !should_output_json(cli, Some(repo.config())) {
                println!("heddle agent: pidfile pointed at dead pid {pid}; cleaned up");
            } else {
                println!("{{\"stopped\":true,\"swept_stale\":true,\"pid\":{pid}}}");
            }
            return Ok(());
        }
        // Identity check — protects against PID reuse after a dirty
        // crash. If the running process at `pid` isn't a heddle binary,
        // the pidfile is stale and we must not signal.
        if !is_heddle_process(pid) {
            let _ = std::fs::remove_file(&pid_path);
            return Err(anyhow!(
                "pid {pid} is alive but does not look like a heddle process \
                 (pidfile sweep performed; rerun `agent stop` if you actually meant to stop the daemon)"
            ));
        }
        // SAFETY: pid validated as alive + identified as heddle just
        // above; SIGTERM lets the daemon's RAII guard remove the pidfile
        // and socket cleanly.
        unsafe {
            if libc::kill(pid as libc::pid_t, libc::SIGTERM) != 0 {
                let err = std::io::Error::last_os_error();
                return Err(anyhow!("failed to signal daemon pid {pid}: {err}"));
            }
        }
        if !should_output_json(cli, Some(repo.config())) {
            println!("heddle agent: SIGTERM sent to pid {pid}");
        } else {
            println!("{{\"stopped\":true,\"swept_stale\":false,\"pid\":{pid}}}");
        }
    }
    #[cfg(not(unix))]
    {
        let _ = raw;
        return Err(anyhow!("heddle agent stop is only supported on Unix"));
    }
    Ok(())
}

fn open_repo() -> Result<Repository> {
    let cwd = std::env::current_dir().context("get current working directory")?;
    Repository::open(&cwd).context("open Heddle repository")
}

#[cfg(unix)]
fn pid_path(repo: &Repository) -> PathBuf {
    default_pid_path(repo.heddle_dir())
}

#[cfg(unix)]
fn socket_path(repo: &Repository) -> PathBuf {
    default_socket_path(repo.heddle_dir())
}

#[cfg(not(unix))]
fn pid_path(_repo: &Repository) -> PathBuf {
    PathBuf::from("/dev/null/heddle-agent-not-supported.pid")
}

#[cfg(not(unix))]
fn socket_path(_repo: &Repository) -> PathBuf {
    PathBuf::from("/dev/null/heddle-agent-not-supported.sock")
}

/// Read the daemon pidfile. Accepts both the legacy single-integer
/// format and the structured `(pid, marker, started_at)` format used by
/// daemons started after this PR — `status` only needs the PID, but the
/// `stop` path additionally checks the marker via [`PidFileContents`].
#[cfg(unix)]
fn read_pid(path: &Path) -> Option<u32> {
    let raw = std::fs::read_to_string(path).ok()?;
    if let Some(structured) = PidFileContents::parse(&raw) {
        return u32::try_from(structured.pid).ok();
    }
    raw.trim().parse::<u32>().ok()
}

#[cfg(not(unix))]
fn read_pid(_path: &Path) -> Option<u32> {
    None
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // SAFETY: kill(pid, 0) returns 0 on success and -1 on missing process.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    false
}