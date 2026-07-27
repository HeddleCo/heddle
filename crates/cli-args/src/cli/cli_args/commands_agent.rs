// SPDX-License-Identifier: Apache-2.0
//! `heddle agent` — agent-loop affordances.
//!
//! Agent commands are one-shot repository operations. The unused local daemon
//! daemon control surface was removed during the native hosted cutover.

use clap::Subcommand;

use super::commands_args::{
    AgentApiListArgs, AgentCaptureArgs, AgentFanoutPlanArgs, AgentFanoutStartArgs,
    AgentHeartbeatArgs, AgentPresenceCompleteArgs, AgentPresenceExplainArgs, AgentPresenceListArgs,
    AgentPresenceShowArgs, AgentProvenanceBeginArgs, AgentProvenanceEndArgs,
    AgentProvenanceListArgs, AgentProvenanceSegmentArgs, AgentProvenanceShowArgs, AgentReadyArgs,
    AgentReleaseArgs, AgentReserveArgs, AgentTaskCreateArgs, AgentTaskListArgs, AgentTaskShowArgs,
    AgentTaskUpdateArgs,
};

#[derive(Clone, Debug, Subcommand)]
pub enum AgentCommands {
    // --- Reservation API (orchestration surface) ----------------
    // The variants below are the stable JSON API that orchestrators
    // (Claude Code subagents, codex harnesses, etc.) call to
    // coordinate parallel writers on the same repo. Distinct from
    // the daemon-control variants above: these are one-shot CLI
    // calls that mutate writer leases and actor presence.
    /// Atomically reserve a thread for one writer.
    Reserve(AgentReserveArgs),

    /// Update reservation heartbeat.
    Heartbeat(AgentHeartbeatArgs),

    /// Capture under a token-authenticated writer lease.
    Capture(AgentCaptureArgs),

    /// Mark a reservation's thread ready for integration.
    Ready(AgentReadyArgs),

    /// Release a reservation (status: complete | abandoned).
    Release(AgentReleaseArgs),

    /// List agent reservations (optionally filtered to alive ones).
    List(AgentApiListArgs),

    /// Manage local agent task assignments.
    #[command(subcommand)]
    Task(AgentTaskCommands),

    /// Plan and start native fan-out lanes.
    #[command(subcommand)]
    Fanout(AgentFanoutCommands),

    /// Inspect attribution and work context for local agents.
    #[command(subcommand)]
    Presence(AgentPresenceCommands),

    /// Record provider, model, and policy provenance across an agent run.
    #[command(subcommand)]
    Provenance(AgentProvenanceCommands),
}

#[derive(Clone, Debug, Subcommand)]
pub enum AgentPresenceCommands {
    /// List agent presence records known to this repository.
    List(AgentPresenceListArgs),

    /// Show the current or selected agent presence record.
    Show(AgentPresenceShowArgs),

    /// Explain why Heddle attached the current or selected presence record.
    Explain(AgentPresenceExplainArgs),

    /// Mark the current or selected presence record complete.
    Complete(AgentPresenceCompleteArgs),
}

#[derive(Clone, Debug, Subcommand)]
pub enum AgentProvenanceCommands {
    /// Begin a provider/model provenance session.
    Begin(AgentProvenanceBeginArgs),

    /// Record a provider, model, or policy change within the current session.
    Segment(AgentProvenanceSegmentArgs),

    /// End the current or selected provenance session.
    End(AgentProvenanceEndArgs),

    /// Show the current or selected provenance session.
    Show(AgentProvenanceShowArgs),

    /// List provenance sessions.
    List(AgentProvenanceListArgs),
}

#[derive(Clone, Debug, Subcommand)]
pub enum AgentTaskCommands {
    /// Create a local agent task assignment.
    Create(AgentTaskCreateArgs),

    /// List local agent task assignments.
    List(AgentTaskListArgs),

    /// Show one local agent task assignment.
    Show(AgentTaskShowArgs),

    /// Update one local agent task assignment.
    Update(AgentTaskUpdateArgs),
}

#[derive(Clone, Debug, Subcommand)]
pub enum AgentFanoutCommands {
    /// Preview fan-out lane setup and return commands without writing.
    Plan(AgentFanoutPlanArgs),

    /// Create task assignments and materialized child lanes.
    Start(AgentFanoutStartArgs),
}
