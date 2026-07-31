// SPDX-License-Identifier: Apache-2.0
//! Local glue between cli's dispatch and the `WeftExtensions`
//! trait surface.
//!
//! In OSS builds without the `client` feature, `main.rs` constructs a
//! `NoopWeftExtensions` from the shim crate directly. With
//! `client` enabled, this module provides
//! [`EnabledWeftExtensions`], which downcasts the trait's opaque
//! arguments back to `cli::cli::AuthCommands` and delegates to the
//! CLI-owned hosted runtime. Closed builds can continue replacing the shim
//! package through `[patch.crates-io]` without owning Heddle's transport.

#![cfg(feature = "client")]

use std::any::Any;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use weft_client_shim::{CliContext, WeftExtensions};

use crate::{
    cli::{AgentTemplateArg, AuthCommands, AuthTrustCommands, commands::cmd_auth},
    hosted_runtime::{
        auth_requests::{AuthCommand, AuthTrustCommand},
        device_flow::AgentTemplate,
    },
};

pub struct EnabledWeftExtensions;

#[async_trait]
impl WeftExtensions for EnabledWeftExtensions {
    async fn auth(
        &self,
        ctx: &(dyn CliContext + 'static),
        command: &(dyn Any + Send + Sync),
    ) -> Result<()> {
        let command = downcast::<AuthCommands>(command, "AuthCommands")?;
        cmd_auth(ctx, auth_command(command.clone())).await
    }

    async fn whoami(&self, ctx: &(dyn CliContext + 'static), server: Option<String>) -> Result<()> {
        crate::hosted_runtime::whoami::cmd_whoami(ctx, server).await
    }
}

fn auth_command(command: AuthCommands) -> AuthCommand {
    match command {
        AuthCommands::Login {
            server,
            open_browser,
            credential,
        } => AuthCommand::Login {
            server,
            open_browser,
            credential,
        },
        AuthCommands::Logout { server } => AuthCommand::Logout { server },
        AuthCommands::Status { server } => AuthCommand::Status { server },
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

fn downcast<'a, T: 'static>(
    value: &'a (dyn Any + Send + Sync),
    name: &'static str,
) -> Result<&'a T> {
    value
        .downcast_ref::<T>()
        .ok_or_else(|| anyhow!("WeftExtensions trait dispatch: failed to downcast to {name}"))
}
