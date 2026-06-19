// SPDX-License-Identifier: Apache-2.0
//! Local glue between cli's dispatch and the `WeftExtensions`
//! trait surface.
//!
//! With `client` enabled, this module provides
//! [`EnabledWeftExtensions`], which downcasts the trait's opaque
//! arguments back to the concrete `cli::cli::AuthCommands` /
//! `SupportCommands` / `PresenceCommands` types and delegates to the
//! existing in-`cli` command implementations.
//!
//! Step 5 of the OSS extraction plan moves the underlying command
//! implementations out of `cli` into a separate `client`
//! crate that ships the closed build. At that point this adapter goes
//! away and the closed crate implements `WeftExtensions` directly.

#![cfg(feature = "client")]

use std::any::Any;

use anyhow::{Result, anyhow};
use weft_client_shim::{CliContext, WeftExtensions, WeftFuture};

use crate::cli::{
    AuthCommands,
    cli_args::SupportCommands,
    commands::{cmd_auth, cmd_presence_publish, cmd_support},
};

pub struct EnabledWeftExtensions;

impl WeftExtensions for EnabledWeftExtensions {
    fn auth<'a>(
        &'a self,
        ctx: &'a (dyn CliContext + 'static),
        command: &'a (dyn Any + Send + Sync),
    ) -> WeftFuture<'a> {
        Box::pin(async move {
            let command = downcast::<AuthCommands>(command, "AuthCommands")?;
            cmd_auth(ctx, command.clone()).await
        })
    }

    fn support<'a>(
        &'a self,
        ctx: &'a (dyn CliContext + 'static),
        command: &'a (dyn Any + Send + Sync),
    ) -> WeftFuture<'a> {
        Box::pin(async move {
            let command = downcast::<SupportCommands>(command, "SupportCommands")?;
            cmd_support(ctx, command.clone()).await
        })
    }

    fn presence_publish<'a>(
        &'a self,
        ctx: &'a (dyn CliContext + 'static),
        session: String,
        interval_secs: u64,
    ) -> WeftFuture<'a> {
        Box::pin(async move { cmd_presence_publish(ctx, session, interval_secs).await })
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
