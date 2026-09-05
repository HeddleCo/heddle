// SPDX-License-Identifier: Apache-2.0
//! `heddle env` — confidential-runtime profiles (ADR 0051 / heddle#999).
//!
//! `run` is the product path: the policy broker unwraps named slots and
//! injects them into a child process. Values never land in the worktree,
//! the store, or command JSON. `create` / `list` are metadata and
//! ciphertext only.

use clap::{Args, Subcommand};

#[derive(Clone, Debug, Subcommand)]
pub enum EnvCommands {
    /// Create a runtime profile from current environment values.
    ///
    /// `--from-env SLOT` copies `SLOT` from this process into ciphertext.
    /// The value is not printed.
    Create(EnvCreateArgs),
    /// List runtime profiles and slot names. Never prints values.
    List(EnvListArgs),
    /// Run a child with profile slots injected as environment variables.
    ///
    /// Plaintext lives in the child only. Same-UID callers are cooperative;
    /// OS process isolation is a later slice.
    #[command(after_help = "\
Examples:
  heddle env run --profile production -- printenv DATABASE_URL
  heddle env run --profile local --slot TOKEN -- env
")]
    Run(EnvRunArgs),
}

#[derive(Clone, Debug, Args)]
pub struct EnvCreateArgs {
    /// Profile name (`[A-Za-z0-9._-]`, 1..=64).
    #[arg(long)]
    pub name: String,

    /// Copy this environment variable into a slot of the same name.
    /// Repeat for multiple slots.
    #[arg(long = "from-env", value_name = "SLOT", required = true)]
    pub from_env: Vec<String>,
}

#[derive(Clone, Debug, Args)]
pub struct EnvListArgs {}

#[derive(Clone, Debug, Args)]
pub struct EnvRunArgs {
    /// Runtime profile name.
    #[arg(long)]
    pub profile: String,

    /// Slot names to inject. Default: every slot on the current head.
    #[arg(long = "slot", value_name = "SLOT")]
    pub slots: Vec<String>,

    /// Request lifetime in seconds. The broker refuses after expiry.
    #[arg(long, default_value_t = 30)]
    pub ttl: u64,

    /// Child command. Use `--` to separate it from Heddle flags.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}
