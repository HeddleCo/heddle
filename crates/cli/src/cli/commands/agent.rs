// SPDX-License-Identifier: Apache-2.0
//! `heddle agent serve|status|stop` handlers.

#[cfg(feature = "local-services")]
use std::path::{Path, PathBuf};

use anyhow::Result;
#[cfg(feature = "local-services")]
use anyhow::{Context, anyhow};
#[cfg(all(unix, feature = "local-services"))]
use daemon::local_daemon::{
    LocalDaemonConfig, PidFileContents, default_pid_path, default_socket_path, is_heddle_process,
    serve,
};
#[cfg(feature = "local-services")]
use repo::Repository;
#[cfg(feature = "local-services")]
use serde::Serialize;

#[cfg(feature = "local-services")]
use super::{
    advice::RecoveryAdvice,
    git_overlay_health::{RepositoryVerificationState, build_repository_verification_state},
};
use crate::cli::cli_args::{AgentCommands, Cli};
#[cfg(feature = "local-services")]
use crate::cli::{cli_args::AgentServeArgs, should_output_json};

#[cfg(feature = "local-services")]
#[derive(Serialize)]
pub(crate) struct AgentServeOutput {
    pub output_kind: &'static str,
    pub status: String,
    pub socket_path: String,
    pub pid_path: String,
    #[allow(dead_code)]
    #[serde(skip_serializing)]
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
}

#[cfg(feature = "local-services")]
#[derive(Serialize)]
pub(crate) struct AgentStatusOutput {
    output_kind: &'static str,
    running: bool,
    pid: Option<u32>,
    socket_path: String,
    pid_path: String,
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

#[cfg(feature = "local-services")]
#[derive(Serialize)]
pub(crate) struct AgentStopOutput {
    output_kind: &'static str,
    stopped: bool,
    swept_stale: bool,
    pid: Option<i32>,
    reason: Option<String>,
    #[allow(dead_code)]
    #[serde(skip_serializing)]
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

pub async fn run(cli: &Cli, command: &AgentCommands) -> Result<()> {
    match command {
        #[cfg(feature = "local-services")]
        AgentCommands::Serve(args) => run_serve(cli, args).await,
        #[cfg(feature = "local-services")]
        AgentCommands::Status => run_status(cli).await,
        #[cfg(feature = "local-services")]
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

#[cfg(feature = "local-services")]
async fn run_serve(cli: &Cli, args: &AgentServeArgs) -> Result<()> {
    #[cfg(not(unix))]
    {
        let _ = (cli, args);
        return Err(anyhow!(RecoveryAdvice::safety_refusal(
            "agent_serve_unsupported_platform",
            "heddle agent serve is only supported on Unix platforms",
            "Use agent reservation commands directly on this platform.",
            "this platform does not support the Unix socket daemon used by `heddle agent serve`",
            "starting the daemon would require unsupported process and socket primitives",
            "no daemon process was started and repository files were left unchanged",
            "heddle agent status",
            vec!["heddle agent status".to_string()],
        )));
    }
    #[cfg(unix)]
    {
        let repo = cli.open_repo()?;
        let mut config = LocalDaemonConfig::from_repo(&repo);
        if let Some(socket) = args.socket.clone() {
            config = config.with_socket(socket);
        }
        if !args.foreground {
            // First-ship simplification: foreground only. Daemonization on
            // Unix needs careful fork+setsid handling and is best layered
            // with a battle-tested helper. The CLI tip nudges users toward
            // `heddle agent serve --foreground &`.
            return Err(anyhow!(RecoveryAdvice::invalid_usage(
                "agent_background_unimplemented",
                "background daemonization is not yet implemented; pass --foreground",
                "Run `heddle agent serve --foreground` and background it from your shell if needed.",
                "heddle agent serve --foreground",
            )));
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
        let socket_path = config.socket_path.display().to_string();
        let pid_path = config.pid_path.display().to_string();
        let repo_root = repo.root().to_path_buf();
        serve(repo, config, shutdown)
            .await
            .map_err(|e| anyhow!("local daemon failed: {e}"))?;
        let repo = Repository::open(&repo_root)?;
        if should_output_json(cli, Some(repo.config())) {
            let output = AgentServeOutput {
                output_kind: "agent_serve",
                status: "stopped".to_string(),
                socket_path,
                pid_path,
                trust: build_repository_verification_state(&repo),
            };
            println!("{}", serde_json::to_string(&output)?);
        }
        Ok(())
    }
}

#[cfg(feature = "local-services")]
async fn run_status(cli: &Cli) -> Result<()> {
    let repo = cli.open_repo()?;
    let pid_path = pid_path(&repo);
    let socket_path = socket_path(&repo);
    let pid = read_pid(&pid_path);
    let running = pid.map(pid_alive).unwrap_or(false);
    let output = AgentStatusOutput {
        output_kind: "agent_status",
        running,
        pid,
        socket_path: socket_path.display().to_string(),
        pid_path: pid_path.display().to_string(),
        trust: build_repository_verification_state(&repo),
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

#[cfg(feature = "local-services")]
async fn run_stop(cli: &Cli) -> Result<()> {
    let repo = cli.open_repo()?;
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
                print_stop_output(
                    &repo,
                    AgentStopOutput {
                        output_kind: "agent_stop",
                        stopped: false,
                        swept_stale: false,
                        pid: None,
                        reason: Some("no pidfile".to_string()),
                        trust: build_repository_verification_state(&repo),
                    },
                )?;
            }
            return Ok(());
        }
    };
    #[cfg(unix)]
    {
        let parsed = match PidFileContents::parse(&raw) {
            Some(c) => c,
            None => {
                return Err(anyhow!(RecoveryAdvice::safety_refusal(
                    "agent_pidfile_invalid",
                    format!(
                        "pidfile at {} is not in the heddle agent format; refusing to send a signal",
                        pid_path.display()
                    ),
                    "Inspect the pidfile and remove it manually only if it is stale.",
                    format!(
                        "{} does not contain a Heddle agent pidfile marker",
                        pid_path.display()
                    ),
                    "sending SIGTERM could stop an unrelated process if the pidfile was not written by Heddle",
                    "the pidfile, socket, daemon process, and repository state were left unchanged",
                    "heddle agent status",
                    vec!["heddle agent status".to_string()],
                )));
            }
        };
        let pid = parsed.pid;
        if !pid_alive(pid as u32) {
            let _ = std::fs::remove_file(&pid_path);
            if !should_output_json(cli, Some(repo.config())) {
                println!("heddle agent: pidfile pointed at dead pid {pid}; cleaned up");
            } else {
                print_stop_output(
                    &repo,
                    AgentStopOutput {
                        output_kind: "agent_stop",
                        stopped: true,
                        swept_stale: true,
                        pid: Some(pid),
                        reason: None,
                        trust: build_repository_verification_state(&repo),
                    },
                )?;
            }
            return Ok(());
        }
        // Identity check — protects against PID reuse after a dirty
        // crash. If the running process at `pid` isn't this executable,
        // the pidfile is stale and we must not signal.
        if !is_heddle_process(pid) {
            let _ = std::fs::remove_file(&pid_path);
            return Err(anyhow!(RecoveryAdvice::safety_refusal(
                "agent_pid_not_heddle",
                format!("pid {pid} is alive but does not match this Heddle executable"),
                "Rerun `heddle agent stop` only if a fresh Heddle agent pidfile appears.",
                format!("pidfile pointed at live pid {pid} with a different executable identity"),
                "sending SIGTERM could stop a process Heddle does not own",
                "the stale pidfile was removed; repository objects, refs, and worktree files were left unchanged",
                "heddle agent status",
                vec!["heddle agent status".to_string()],
            )));
        }
        // SAFETY: pid validated as alive + identified as heddle just
        // above; SIGTERM lets the daemon's RAII guard remove the pidfile
        // and socket cleanly.
        unsafe {
            if libc::kill(pid as libc::pid_t, libc::SIGTERM) != 0 {
                let err = std::io::Error::last_os_error();
                return Err(anyhow!(RecoveryAdvice::safety_refusal(
                    "agent_signal_failed",
                    format!("failed to signal daemon pid {pid}: {err}"),
                    "Run `heddle agent status` to inspect the recorded daemon before retrying.",
                    format!("the OS refused SIGTERM for recorded daemon pid {pid}: {err}"),
                    "retrying blindly could race daemon shutdown or PID reuse",
                    "the pidfile, socket, daemon process, and repository state were left unchanged",
                    "heddle agent status",
                    vec!["heddle agent status".to_string()],
                )));
            }
        }
        if !should_output_json(cli, Some(repo.config())) {
            println!("heddle agent: SIGTERM sent to pid {pid}");
        } else {
            print_stop_output(
                &repo,
                AgentStopOutput {
                    output_kind: "agent_stop",
                    stopped: true,
                    swept_stale: false,
                    pid: Some(pid),
                    reason: None,
                    trust: build_repository_verification_state(&repo),
                },
            )?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = raw;
        return Err(anyhow!(RecoveryAdvice::safety_refusal(
            "agent_stop_unsupported_platform",
            "heddle agent stop is only supported on Unix",
            "Use agent reservation commands directly on this platform.",
            "this platform does not support Unix SIGTERM for the Heddle agent daemon",
            "stopping the daemon would require unsupported process signalling",
            "no daemon process was signalled and repository files were left unchanged",
            "heddle agent status",
            vec!["heddle agent status".to_string()],
        )));
    }
    Ok(())
}

#[cfg(feature = "local-services")]
fn print_stop_output(_repo: &Repository, output: AgentStopOutput) -> Result<()> {
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

#[cfg(all(unix, feature = "local-services"))]
fn pid_path(repo: &Repository) -> PathBuf {
    default_pid_path(repo.heddle_dir())
}

#[cfg(all(unix, feature = "local-services"))]
fn socket_path(repo: &Repository) -> PathBuf {
    default_socket_path(repo.heddle_dir())
}

#[cfg(all(not(unix), feature = "local-services"))]
fn pid_path(_repo: &Repository) -> PathBuf {
    PathBuf::from("/dev/null/heddle-agent-not-supported.pid")
}

#[cfg(all(not(unix), feature = "local-services"))]
fn socket_path(_repo: &Repository) -> PathBuf {
    PathBuf::from("/dev/null/heddle-agent-not-supported.sock")
}

/// Read the daemon pidfile. Accepts both the legacy single-integer
/// format and the structured `(pid, marker, started_at)` format used by
/// daemons started after this PR — `status` only needs the PID, but the
/// `stop` path additionally checks the marker via [`PidFileContents`].
#[cfg(all(unix, feature = "local-services"))]
fn read_pid(path: &Path) -> Option<u32> {
    let raw = std::fs::read_to_string(path).ok()?;
    if let Some(structured) = PidFileContents::parse(&raw) {
        return u32::try_from(structured.pid).ok();
    }
    raw.trim().parse::<u32>().ok()
}

#[cfg(all(not(unix), feature = "local-services"))]
fn read_pid(_path: &Path) -> Option<u32> {
    None
}

#[cfg(all(unix, feature = "local-services"))]
fn pid_alive(pid: u32) -> bool {
    // SAFETY: kill(pid, 0) returns 0 on success and -1 on missing process.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(all(not(unix), feature = "local-services"))]
fn pid_alive(_pid: u32) -> bool {
    false
}
