// SPDX-License-Identifier: Apache-2.0
//! Trait surface separating the OSS Heddle CLI from the closed
//! hosted-client implementation.
//!
//! OSS builds use [`NoopWeftExtensions`], which returns a friendly
//! "hosted features not enabled" error on every method. Closed builds
//! ship a real implementation in the `hosted-client` crate and inject
//! it via Cargo features (today) or `[patch.crates-io]` (post-split).
//!
//! Why a separate crate (and not just a trait in `cli`)? When the
//! repos physically split, the OSS `heddle-cli` crate ships on
//! crates.io. The closed `heddle-hosted-client` crate published in the
//! private workspace depends on this shim to satisfy `cli`'s trait
//! bound without `cli` ever knowing about closed-source code. Same
//! trait surface, two impls, no circular deps.
//!
//! Trait methods are intentionally minimal — only the truly
//! hosted-only commands (`auth`, `support`, `presence`) flow through
//! here. Hybrid commands like `push`/`pull`/`fetch`/`clone` stay in
//! `cli` because their git-overlay-without-hosted code paths must
//! work in OSS-only builds too.

use std::any::Any;
use std::path::Path;

use anyhow::{Result, anyhow};
use async_trait::async_trait;

/// Small projection of `cli::Cli` that hosted commands rely on.
/// Defining the surface here rather than passing `&Cli` lets the
/// closed `hosted-client` crate compile without depending on `cli` —
/// breaking what would otherwise be a circular dep (cli optionally
/// pulls in hosted-client, hosted-client would otherwise need cli for
/// the `Cli` type).
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

    /// `--op-id` override for idempotent gRPC calls. Empty string
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
/// build time by the `hosted-client` Cargo feature.
///
/// Implementations take CLI args opaquely (`&dyn Any`) so this shim
/// crate doesn't need to depend on `cli` for type definitions —
/// downstream concrete impls downcast to the real types. This avoids
/// a circular dependency between `cli` (which defines `Cli`,
/// `AuthCommands`, etc.) and the hosted-client crate.
#[async_trait]
pub trait WeftExtensions: Send + Sync {
    /// `heddle auth <subcommand>` — login, logout, whoami, device
    /// authorization, service account issuance.
    async fn auth(
        &self,
        ctx: &(dyn CliContext + 'static),
        command: &(dyn Any + Send + Sync),
    ) -> Result<()>;

    /// `heddle support <subcommand>` — hosted-side support and
    /// diagnostic operations.
    async fn support(
        &self,
        ctx: &(dyn CliContext + 'static),
        command: &(dyn Any + Send + Sync),
    ) -> Result<()>;

    /// `heddle presence publish` — stream presence/heartbeat over the
    /// websocket transport to the hosted backend.
    async fn presence_publish(
        &self,
        ctx: &(dyn CliContext + 'static),
        session: String,
        interval_secs: u64,
    ) -> Result<()>;
}

/// Noop implementation used in OSS builds. Every method returns the
/// same friendly error pointing the user at the closed-build
/// installation path.
pub struct NoopWeftExtensions;

#[async_trait]
impl WeftExtensions for NoopWeftExtensions {
    async fn auth(
        &self,
        _ctx: &(dyn CliContext + 'static),
        _command: &(dyn Any + Send + Sync),
    ) -> Result<()> {
        Err(anyhow!(not_enabled_error("auth")))
    }

    async fn support(
        &self,
        _ctx: &(dyn CliContext + 'static),
        _command: &(dyn Any + Send + Sync),
    ) -> Result<()> {
        Err(anyhow!(not_enabled_error("support")))
    }

    async fn presence_publish(
        &self,
        _ctx: &(dyn CliContext + 'static),
        _session: String,
        _interval_secs: u64,
    ) -> Result<()> {
        Err(anyhow!(not_enabled_error("presence publish")))
    }
}

fn not_enabled_error(command: &str) -> String {
    format!(
        "`heddle {command}` requires the hosted-client build of Heddle. \
         Install it from https://heddleco.com or rebuild the CLI with \
         `--features hosted-client` if you're working from source."
    )
}