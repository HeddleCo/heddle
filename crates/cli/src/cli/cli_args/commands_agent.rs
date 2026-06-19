// SPDX-License-Identifier: Apache-2.0
//! `heddle agent` — agent-loop affordances.
//!
//! `agent serve` runs a local-only gRPC daemon
//! over a Unix-domain socket inside the repo's `.heddle/sockets/`. The
//! daemon hosts agent services (state-review, discussion, signal,
//! operation-log query, transaction, hook) so a tight agent loop avoids
//! per-command process startup latency.
//!
//! `heddle agent serve` is intentionally distinct from `heddle daemon`,
//! which controls the FUSE mount daemon — different subsystem, different
//! UX. `heddle help daemon` contrasts the two.

#[cfg(feature = "local-services")]
use std::path::PathBuf;

use clap::Subcommand;

use super::commands_args::{
    AgentApiListArgs, AgentCaptureArgs, AgentHeartbeatArgs, AgentReadyArgs, AgentReleaseArgs,
    AgentReserveArgs,
};

#[derive(Clone, Debug, Subcommand)]
pub enum AgentCommands {
    /// Run the local agent gRPC daemon.
    ///
    /// The daemon binds a Unix socket inside the repo's `.heddle/sockets/`
    /// directory and serves local same-user CLI calls.
    #[cfg(feature = "local-services")]
    Serve(AgentServeArgs),

    /// Report whether the local agent daemon is running for this repo.
    #[cfg(feature = "local-services")]
    Status,

    /// Ask the running daemon to drain and exit.
    #[cfg(feature = "local-services")]
    Stop,

    // --- Reservation API (orchestration surface) ----------------
    // The variants below are the stable JSON API that orchestrators
    // (Claude Code subagents, codex harnesses, etc.) call to
    // coordinate parallel writers on the same repo. Distinct from
    // the daemon-control variants above: these are one-shot CLI
    // calls that mutate the repo's `.heddle/agents/` registry.
    /// Atomically reserve a thread for one writer.
    Reserve(AgentReserveArgs),

    /// Update reservation heartbeat.
    Heartbeat(AgentHeartbeatArgs),

    /// Capture under a session-validated reservation.
    Capture(AgentCaptureArgs),

    /// Mark a reservation's thread ready for integration.
    Ready(AgentReadyArgs),

    /// Release a reservation (status: complete | abandoned).
    Release(AgentReleaseArgs),

    /// List agent reservations (optionally filtered to alive ones).
    List(AgentApiListArgs),
}

#[cfg(feature = "local-services")]
#[derive(Clone, Debug, clap::Args)]
pub struct AgentServeArgs {
    /// Override the default socket path (`<heddle_dir>/sockets/grpc.sock`).
    #[arg(long)]
    pub socket: Option<PathBuf>,
    /// Run in the foreground; without this the daemon detaches and writes
    /// its pidfile so the parent shell returns immediately.
    #[arg(long)]
    pub foreground: bool,
}
