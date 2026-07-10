// SPDX-License-Identifier: Apache-2.0
//! `heddle prove` command arguments (handle wave F1b).
//!
//! Git-native identity proofs over the `HostedUserService`
//! `RequestProofChallenge` / `SubmitProof` / `ListProofs` RPCs (the client
//! half of weft's F1a engine). Each subcommand resolves the hosted server from
//! a named remote (default `origin`), mirroring the `heddle spool` / `heddle
//! thread approve` hosted surface.

use clap::{Args, Subcommand};

#[derive(Clone, Debug, Subcommand)]
pub enum ProveCommands {
    /// Submit a started challenge for verification.
    ///
    /// The server fetches the marker line from the well-known path in your
    /// repo and reports whether the proof verified.
    Submit(ProveSubmitArgs),

    /// List your git-native identity proofs.
    List(ProveListArgs),
}

/// `heddle prove <host> <repo>` — start a proof (no subcommand). When no
/// subcommand is given, these positional args request a fresh challenge.
#[derive(Clone, Debug, Args)]
pub struct ProveArgs {
    #[command(subcommand)]
    pub command: Option<ProveCommands>,

    /// External host the repo lives on, e.g. `github.com`.
    ///
    /// Required when starting a proof (no subcommand).
    pub host: Option<String>,

    /// The repo you will publish the proof to, e.g. `owner/repo`.
    ///
    /// Required when starting a proof (no subcommand). Named `repo_spec` to
    /// avoid colliding with the global `-C/--repo` path flag.
    #[arg(value_name = "REPO")]
    pub repo_spec: Option<String>,

    /// Also write the marker file to this local path (opt-in convenience).
    ///
    /// Off by default: publishing the proof is your action. When set, the CLI
    /// writes the marker line to the given path so you can commit + push it;
    /// the CLI never pushes to your repo.
    #[arg(long, value_name = "PATH")]
    pub write_file: Option<String>,

    /// Remote that maps to the hosted server (default: `origin`).
    #[arg(long, default_value = "origin")]
    pub remote: String,
}

#[derive(Clone, Debug, Args)]
pub struct ProveSubmitArgs {
    /// The challenge id printed by `heddle prove <host> <repo>`.
    pub challenge_id: String,
    /// Remote that maps to the hosted server (default: `origin`).
    #[arg(long, default_value = "origin")]
    pub remote: String,
}

#[derive(Clone, Debug, Args)]
pub struct ProveListArgs {
    /// Remote that maps to the hosted server (default: `origin`).
    #[arg(long, default_value = "origin")]
    pub remote: String,
}
