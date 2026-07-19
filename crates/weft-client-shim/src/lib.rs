// SPDX-License-Identifier: Apache-2.0
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

//! Trait surface separating the OSS Heddle CLI from the closed
//! heddle-client implementation.
//!
//! OSS builds use [`NoopWeftExtensions`], which returns a friendly
//! "hosted features not enabled" error on every method. Closed builds
//! ship a real implementation in the `heddle-client` crate and inject
//! it via Cargo features (today) or `[patch.crates-io]` (post-split).
//!
//! Why a separate crate (and not just a trait in `cli`)? When the
//! repos physically split, the OSS `heddle-cli` crate ships on
//! crates.io. The closed `heddle-client` crate published in the
//! private workspace depends on this shim to satisfy `cli`'s trait
//! bound without `cli` ever knowing about closed-source code. Same
//! trait surface, two impls, no circular deps.
//!
//! Trait methods are intentionally minimal — only the truly
//! hosted-only commands (`auth`, `support`, `presence`) flow through
//! here. Hybrid commands like `push`/`pull`/`fetch`/`clone` stay in
//! `cli` because their git-overlay-without-hosted code paths must
//! work in OSS-only builds too.

use std::{any::Any, error::Error, fmt, path::Path};

use anyhow::{Result, anyhow};
use async_trait::async_trait;

/// Small projection of `cli::Cli` that hosted commands rely on.
/// Defining the surface here rather than passing `&Cli` lets the
/// closed `heddle-client` crate compile without depending on `cli` —
/// breaking what would otherwise be a circular dep (cli optionally
/// pulls in heddle-client, heddle-client would otherwise need cli for
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

/// Typed hosted-command recovery advice that the OSS CLI can render as the
/// same JSON/text envelope used by native commands without depending on the
/// hosted client implementation.
#[derive(Debug, Clone)]
pub struct HostedRecoveryAdvice {
    pub kind: &'static str,
    pub error: String,
    pub hint: String,
    pub unsafe_condition: String,
    pub would_change: String,
    pub preserved: String,
    pub primary_command: String,
    pub recovery_commands: Vec<String>,
}

impl HostedRecoveryAdvice {
    pub fn invalid_usage(
        kind: &'static str,
        error: impl Into<String>,
        hint: impl Into<String>,
        primary_command: impl Into<String>,
    ) -> Self {
        let primary_command = primary_command.into();
        Self {
            kind,
            error: error.into(),
            hint: hint.into(),
            unsafe_condition: "the command arguments do not describe a valid hosted operation"
                .to_string(),
            would_change:
                "running with ambiguous or invalid arguments could target the wrong hosted resource"
                    .to_string(),
            preserved: "no hosted request was sent and local repository state was left unchanged"
                .to_string(),
            primary_command: primary_command.clone(),
            recovery_commands: vec![primary_command],
        }
    }

    pub fn auth_required(server: &str) -> Self {
        let primary_command = format!("heddle auth login --server {server}");
        Self {
            kind: "auth_required",
            error: format!("Not authenticated with {server}"),
            hint: format!(
                "Run `{primary_command}` to authenticate, then retry the hosted command."
            ),
            unsafe_condition: "no usable hosted credential is available for the selected server"
                .to_string(),
            would_change:
                "continuing without credentials would send an unauthenticated hosted mutation"
                    .to_string(),
            preserved: "no hosted request was sent and local repository state was left unchanged"
                .to_string(),
            primary_command: primary_command.clone(),
            recovery_commands: vec![primary_command],
        }
    }

    pub fn hosted_remote_required(remote: &str, feature: &str) -> Self {
        Self {
            kind: "hosted_remote_required",
            error: format!("{feature} requires a hosted remote; remote '{remote}' is local"),
            hint: "Configure a hosted remote or retry against one that resolves to a network target."
                .to_string(),
            unsafe_condition: format!("remote '{remote}' is local, but {feature} runs on the hosted server"),
            would_change:
                "running locally would imply a hosted policy or support change that no server recorded"
                    .to_string(),
            preserved: "no hosted request was sent and local repository state was left unchanged"
                .to_string(),
            primary_command: "heddle remote list".to_string(),
            recovery_commands: vec!["heddle remote list".to_string()],
        }
    }
}

impl fmt::Display for HostedRecoveryAdvice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}. Unsafe: {}. Would change: {}. Preserved: {}. Primary recovery: `{}`.",
            self.error,
            self.unsafe_condition,
            self.would_change,
            self.preserved,
            self.primary_command
        )
    }
}

impl Error for HostedRecoveryAdvice {}

/// Hosted-side command implementations. The CLI dispatches through a
/// `&dyn WeftExtensions` reference; the active impl is selected at
/// build time by the `heddle-client` Cargo feature.
///
/// Implementations take CLI args opaquely (`&dyn Any`) so this shim
/// crate doesn't need to depend on `cli` for type definitions —
/// downstream concrete impls downcast to the real types. This avoids
/// a circular dependency between `cli` (which defines `Cli`,
/// `AuthCommands`, etc.) and the heddle-client crate.
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
    /// `server` is the optional `--server` override.
    async fn whoami(
        &self,
        ctx: &(dyn CliContext + 'static),
        server: Option<String>,
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

    async fn whoami(
        &self,
        _ctx: &(dyn CliContext + 'static),
        _server: Option<String>,
    ) -> Result<()> {
        Err(anyhow!(not_enabled_error("whoami")))
    }
}

fn not_enabled_error(command: &str) -> String {
    format!(
        "`heddle {command}` requires the client build of Heddle. \
         Install it from https://heddleco.com or rebuild the CLI with \
         `--features client` if you're working from source."
    )
}
