// SPDX-License-Identifier: Apache-2.0
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

//! Patchable extension trait surface for the Heddle CLI.
//!
//! Client builds provide the active implementation. Closed builds may replace
//! this package through `[patch.crates-io]` while the Heddle CLI keeps ownership
//! of its native transport and download runtime.
//!
//! Why a separate crate (and not just a trait in `cli`)? When the
//! repos physically split, the OSS `heddle-cli` crate ships on
//! crates.io. A closed replacement can preserve the trait identity without
//! making `cli` depend on closed-source code. Same trait surface, two impls,
//! no circular deps.
//!
//! Trait methods are intentionally minimal — only the patchable hosted command
//! hooks (`auth` and `whoami`) flow through here. Hybrid commands like
//! `push`/`pull`/`fetch`/`clone` stay in
//! `cli` because their git-overlay-without-hosted code paths must
//! work in OSS-only builds too.

use std::{any::Any, path::Path};

use anyhow::Result;
use async_trait::async_trait;

/// Small projection of `cli::Cli` that hosted commands rely on.
/// Defining the surface here rather than passing `&Cli` lets the
/// a patched extension implementation compile without depending on `cli` and
/// therefore without creating a cycle around the `Cli` type.
///
/// Keep this trait deliberately small. Every new method is a
/// permanent contract between the OSS and closed sides; before adding
/// one, ask whether the hosted command should really need that
/// context at all, or whether the caller can compute it and pass a
/// primitive value.
pub trait CliContext: Send + Sync {
    /// `--repo` override; `None` means "use the process's current
    /// directory."
    fn repo_path(&self) -> Option<&Path>;

    /// `--op-id` override for idempotent hosted calls. Empty string
    /// means the caller did not supply one and the server should not
    /// dedupe.
    fn operation_id_wire(&self) -> String;

    /// Resolves whether output should be JSON, encapsulating the
    /// precedence between the `--json` / `--output` cli flags, the
    /// user's global config, and (when supplied) the repo's
    /// `output.format` config. Hosted commands typically pass
    /// `Some(repo.config())` after opening the repo and `None`
    /// otherwise.
    fn should_output_json(&self, repo_config: Option<&repo::Config>) -> bool;
}

/// Hosted-side command implementations. The CLI dispatches through a
/// `&dyn WeftExtensions` reference; the active impl is selected at
/// build time.
///
/// Implementations take CLI args opaquely (`&dyn Any`) so this shim
/// crate doesn't need to depend on `cli` for type definitions —
/// downstream concrete impls downcast to the real types. This avoids
/// a circular dependency between `cli` (which defines `Cli`,
/// `AuthCommands`, etc.) and a patched extension implementation.
#[async_trait]
pub trait WeftExtensions: Send + Sync {
    /// `heddle auth <subcommand>` — login, logout, device
    /// authorization, service account issuance.
    async fn auth(
        &self,
        ctx: &(dyn CliContext + 'static),
        command: &(dyn Any + Send + Sync),
    ) -> Result<()>;

    /// `heddle whoami` — resolve and report the acting identity (principal,
    /// token kind, scopes, operation ceiling, TTL, signing + reachability).
    /// `server` is the optional `--server` override. Observe-only: never
    /// attaches a credential.
    async fn whoami(&self, ctx: &(dyn CliContext + 'static), server: Option<String>) -> Result<()>;
}
