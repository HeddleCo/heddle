// SPDX-License-Identifier: Apache-2.0
//! Harness integration command definitions.

use clap::Subcommand;

#[derive(Subcommand, Clone, Debug)]
pub enum IntegrationCommands {
    /// List Heddle-managed harness integrations.
    List,

    /// Install harness integrations.
    Install(IntegrationInstallArgs),

    /// Verify installed harness integrations.
    Doctor,

    /// Uninstall harness integrations.
    Uninstall(IntegrationTargetArgs),

    /// Rewrite Heddle-managed integrations in place.
    Upgrade(IntegrationTargetArgs),

    /// Internal relay invoked by installed hooks/plugins.
    #[command(hide = true)]
    Relay(IntegrationRelayArgs),
}

#[derive(Clone, Debug, clap::Args)]
pub struct IntegrationInstallArgs {
    /// Harness names to install (`codex`, `claude-code`, `opencode`). Empty means detected set.
    pub harnesses: Vec<String>,

    /// Install scope (`repo` or `user`).
    #[arg(long, visible_alias = "harness-install-scope", default_value = "repo")]
    pub scope: String,

    /// Overwrite Heddle-managed entries when needed.
    #[arg(long)]
    pub force: bool,

    /// Bake the absolute path of the running heddle binary into the hook commands
    /// instead of the default PATH-relative `heddle`. Useful when pinning to a
    /// specific build (e.g. on a multi-version host).
    #[arg(long)]
    pub absolute_path: bool,
}

#[derive(Clone, Debug, clap::Args)]
pub struct IntegrationTargetArgs {
    /// Harness names to target. Empty means all Heddle-managed harnesses.
    pub harnesses: Vec<String>,
}

#[derive(Clone, Debug, clap::Args)]
pub struct IntegrationRelayArgs {
    /// Harness name.
    pub harness: String,

    /// Event name.
    pub event: String,
}
