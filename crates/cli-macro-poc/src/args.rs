// SPDX-License-Identifier: Apache-2.0
//! Input shape (clap) — a FAITHFUL mirror of the real init args.
//!
//! This PoC mirrors the real `crates/cli/src/cli/cli_args/commands_args.rs:12`
//! `InitArgs`, which derives **`clap::Args`** (NOT `clap::Parser`) and is wired
//! as a tuple payload of the top-level subcommand enum
//! (`Commands::Init(InitArgs)`). clap's derive model reserves `Parser` for the
//! top-level parser and `Args` for reusable argument sets merged into a parent
//! — subcommand tuple-variant payloads must implement `Args`. An earlier draft
//! of this PoC derived `Parser` here, which measured the wrong clap shape: a
//! standalone parser the existing `Commands::Init(_)` variant cannot consume.
//!
//! The spike question for the input side is NOT "can a macro emit a clap
//! struct" (clap's own derive already does that) but "can ONE declaration emit
//! BOTH this clap args struct AND the output schema". `InitArgs` is the input
//! half of that single declaration; [`super::output::InitOutput`] is the output
//! half. The `Cli` / `Commands` wiring below proves the args struct slots into
//! a real subcommand tree — exactly the integration #205's `HeddleVerbArgs`
//! passthrough must preserve.
//!
//! COMPLETE FAITHFUL MIRROR: this `InitArgs` reproduces every derive, every
//! `#[command(...)]` attribute (incl. the `after_help` examples block), and
//! every field with its `#[arg(...)]` attributes from the real
//! `commands_args.rs:5-39` `InitArgs`. The spike uses this crate to prove the
//! `HeddleVerbArgs` passthrough preserves clap behavior — including help text —
//! so the after_help block is load-bearing, not decoration.

use clap::{Args, Parser, Subcommand};

/// Top-level parser — `Parser` lives HERE (and only here), mirroring the real
/// `Cli`. Verb-args types stay on `Args`.
#[derive(Debug, Parser)]
#[command(name = "heddle", bin_name = "heddle")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

/// Subcommand enum whose tuple variants carry `clap::Args` payloads, mirroring
/// the real `Commands` enum.
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Initialize Heddle metadata in a repository.
    Init(InitArgs),
}

/// Arguments for the `init` command. Derives **`clap::Args`** — the reusable
/// argument set the subcommand enum consumes — copied from the real `InitArgs`.
#[derive(Clone, Debug, Args)]
#[command(after_help = "\
Examples:
  heddle init                                  # initialize the current directory
  heddle init my-project                       # initialize a subdirectory
  heddle init --principal-name 'Ada Lovelace'  # set attribution at init time
")]
pub struct InitArgs {
    /// Directory to initialize (default: current directory).
    pub path: Option<std::path::PathBuf>,

    /// Principal name for attribution.
    #[arg(long)]
    pub principal_name: Option<String>,

    /// Principal email for attribution.
    #[arg(long)]
    pub principal_email: Option<String>,

    /// Install harness integrations after init.
    #[arg(long)]
    pub install_harnesses: Option<String>,

    /// Skip harness integration install prompts.
    #[arg(long)]
    pub no_harness_install: bool,

    /// Preferred install scope (`repo` or `user`).
    #[arg(long, visible_alias = "scope", default_value = "repo")]
    pub harness_install_scope: String,

    /// Overwrite Heddle-managed integration entries when needed.
    #[arg(long)]
    pub harness_install_force: bool,
}
