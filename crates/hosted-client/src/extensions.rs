// SPDX-License-Identifier: Apache-2.0
//! Hosted command dispatch.
//!
//! The CLI's `auth` and `whoami` verbs delegate through the
//! [`HostedExtensions`] surface; [`EnabledHostedExtensions`] is the in-repo
//! implementation backed by this crate's hosted runtime. Arguments are the
//! concrete clap types from `heddle-cli-args` — no downcasts.

#![cfg(feature = "client")]

use anyhow::Result;
use async_trait::async_trait;
use heddle_cli_args::{
    AgentTemplateArg, AuthCommands, AuthInviteCommands, AuthTrustCommands, ClaimArgs, CliContext,
};

use crate::hosted_runtime::{
    auth_requests::{AuthCommand, AuthTrustCommand},
    device_flow::AgentTemplate,
    whoami::cmd_whoami,
};

#[async_trait]
pub trait HostedExtensions: Send + Sync {
    /// `heddle auth <subcommand>` — login, logout, device authorization,
    /// service account issuance.
    async fn auth(&self, ctx: &(dyn CliContext + 'static), command: AuthCommands) -> Result<()>;

    /// `heddle claim` — activate and serve a resident agent-account offer.
    async fn claim(&self, args: ClaimArgs) -> Result<()>;

    /// `heddle whoami` — resolve and report the acting identity without
    /// attaching or replacing a credential.
    async fn whoami(&self, ctx: &(dyn CliContext + 'static), server: Option<String>) -> Result<()>;
}

pub struct EnabledHostedExtensions;

#[async_trait]
impl HostedExtensions for EnabledHostedExtensions {
    async fn auth(&self, ctx: &(dyn CliContext + 'static), command: AuthCommands) -> Result<()> {
        let command = auth_command(command);
        crate::hosted_runtime::auth::cmd_auth(ctx, command).await
    }

    async fn claim(&self, args: ClaimArgs) -> Result<()> {
        crate::hosted_runtime::claim_offer::cmd_claim(args).await
    }

    async fn whoami(&self, ctx: &(dyn CliContext + 'static), server: Option<String>) -> Result<()> {
        cmd_whoami(ctx, server).await
    }
}

fn auth_command(command: AuthCommands) -> AuthCommand {
    match command {
        AuthCommands::Login {
            server,
            open_browser,
            invite,
            credential,
        } => AuthCommand::Login {
            server,
            open_browser,
            invite,
            credential,
        },
        AuthCommands::Logout { server } => AuthCommand::Logout { server },
        AuthCommands::Status { server } => AuthCommand::Status { server },
        AuthCommands::Invite {
            email,
            server,
            command,
        } => AuthCommand::Invite {
            email,
            server,
            list: matches!(command, Some(AuthInviteCommands::List)),
        },
        AuthCommands::Trust { command } => AuthCommand::Trust {
            command: match command {
                AuthTrustCommands::Show(args) => AuthTrustCommand::Show {
                    server: args.server,
                },
                AuthTrustCommands::Replace(args) => AuthTrustCommand::Replace {
                    server: args.server,
                    expected_current_public_key: args.expect_current_public_key,
                    key_id: args.key_id,
                    public_key: args.public_key,
                },
            },
        },
        AuthCommands::DeriveAgent {
            server,
            agent_id,
            ttl_secs,
            scopes,
            allowed_operations,
            template,
            out,
        } => AuthCommand::DeriveAgent {
            server,
            agent_id,
            ttl_secs,
            scopes,
            allowed_operations,
            template: template.map(agent_template),
            out,
        },
        AuthCommands::CreateServiceToken {
            name,
            namespace,
            server,
            out,
        } => AuthCommand::CreateServiceToken {
            name,
            namespace,
            server,
            out,
        },
    }
}

fn agent_template(template: AgentTemplateArg) -> AgentTemplate {
    match template {
        AgentTemplateArg::Reviewer => AgentTemplate::Reviewer,
        AgentTemplateArg::Contributor => AgentTemplate::Contributor,
        AgentTemplateArg::CiLanding => AgentTemplate::CiLanding,
    }
}
