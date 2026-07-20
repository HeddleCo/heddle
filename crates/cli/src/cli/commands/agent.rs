// SPDX-License-Identifier: Apache-2.0
//! `heddle agent` one-shot orchestration handlers.

use anyhow::Result;

use crate::cli::cli_args::{
    AgentCommands, AgentPresenceCommands, AgentProvenanceBeginArgs, AgentProvenanceCommands,
    AgentProvenanceEndArgs, AgentProvenanceListArgs, AgentProvenanceSegmentArgs,
    AgentProvenanceShowArgs, Cli,
};

pub async fn run(cli: &Cli, command: &AgentCommands) -> Result<()> {
    match command {
        AgentCommands::Reserve(args) => super::agent_cmd::cmd_agent_reserve(cli, args.clone()),
        AgentCommands::Heartbeat(args) => super::agent_cmd::cmd_agent_heartbeat(cli, args.clone()),
        AgentCommands::Capture(args) => {
            super::agent_cmd::cmd_agent_capture(cli, args.clone()).await
        }
        AgentCommands::Ready(args) => super::agent_cmd::cmd_agent_ready(cli, args.clone()).await,
        AgentCommands::Release(args) => super::agent_cmd::cmd_agent_release(cli, args.clone()),
        AgentCommands::List(args) => super::agent_cmd::cmd_agent_list(cli, args.clone()),
        AgentCommands::Task(command) => super::agent_cmd::cmd_agent_task(cli, command.clone()),
        AgentCommands::Fanout(command) => super::agent_cmd::cmd_agent_fanout(cli, command.clone()),
        AgentCommands::Presence(command) => run_presence(cli, command).await,
        AgentCommands::Provenance(command) => run_provenance(cli, command).await,
    }
}

async fn run_presence(cli: &Cli, command: &AgentPresenceCommands) -> Result<()> {
    match command {
        AgentPresenceCommands::List(args) => super::agent_presence::list(cli, args.active).await,
        AgentPresenceCommands::Show(args) => {
            super::agent_presence::show(cli, args.session.clone()).await
        }
        AgentPresenceCommands::Explain(args) => {
            super::agent_presence::explain(cli, args.session.clone()).await
        }
        AgentPresenceCommands::Complete(args) => {
            super::agent_presence::complete(cli, args.session.clone()).await
        }
    }
}

async fn run_provenance(cli: &Cli, command: &AgentProvenanceCommands) -> Result<()> {
    match command {
        AgentProvenanceCommands::Begin(AgentProvenanceBeginArgs {
            provider,
            model,
            policy,
        }) => {
            super::agent_provenance::begin(cli, provider.clone(), model.clone(), policy.clone())
                .await
        }
        AgentProvenanceCommands::Segment(AgentProvenanceSegmentArgs {
            provider,
            model,
            policy,
        }) => {
            super::agent_provenance::segment(cli, provider.clone(), model.clone(), policy.clone())
                .await
        }
        AgentProvenanceCommands::End(AgentProvenanceEndArgs { session_id }) => {
            super::agent_provenance::end(cli, session_id.clone()).await
        }
        AgentProvenanceCommands::Show(AgentProvenanceShowArgs { session_id }) => {
            super::agent_provenance::show(cli, session_id.clone()).await
        }
        AgentProvenanceCommands::List(AgentProvenanceListArgs { active }) => {
            super::agent_provenance::list(cli, *active).await
        }
    }
}
