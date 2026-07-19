// SPDX-License-Identifier: Apache-2.0
//! Local glue between cli's dispatch and the `WeftExtensions`
//! trait surface.
//!
//! In OSS builds without the `client` feature, `main.rs` constructs a
//! `NoopWeftExtensions` from the shim crate directly. With
//! `client` enabled, this module provides
//! [`EnabledWeftExtensions`], which downcasts the trait's opaque
//! arguments back to `cli::cli::AuthCommands` and delegates to the
//! hosted client implementation.
//!
//! Step 5 of the OSS extraction plan moves the underlying command
//! implementations out of `cli` into a separate `client`
//! crate that ships the closed build. At that point this adapter goes
//! away and the closed crate implements `WeftExtensions` directly.

#![cfg(feature = "client")]

use std::any::Any;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use weft_client_shim::{CliContext, WeftExtensions};

use crate::cli::{AuthCommands, commands::cmd_auth};

pub struct EnabledWeftExtensions;

#[async_trait]
impl WeftExtensions for EnabledWeftExtensions {
    async fn auth(
        &self,
        ctx: &(dyn CliContext + 'static),
        command: &(dyn Any + Send + Sync),
    ) -> Result<()> {
        let command = downcast::<AuthCommands>(command, "AuthCommands")?;
        cmd_auth(ctx, command.clone().into()).await
    }

    async fn whoami(
        &self,
        ctx: &(dyn CliContext + 'static),
        server: Option<String>,
    ) -> Result<()> {
        heddle_client::cmd_whoami(ctx, server).await
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
