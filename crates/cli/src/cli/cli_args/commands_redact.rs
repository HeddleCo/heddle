// SPDX-License-Identifier: Apache-2.0
//! `heddle redact` and `heddle purge` — the redaction primitive.
//!
//! See `docs/PRINCIPLES.md` and the build brief at
//! `.agents/redaction-primitive.md` for the design rationale. Briefly:
//!
//! - `redact` declares a blob redacted; readers see a stub on
//!   materialize, but the bytes remain on disk.
//! - `purge` removes the bytes from local storage. The `Redaction`
//!   tombstone stays in the DAG forever.
//!
//! Both verbs write `OpRecord` entries (`Redact`, `Purge`) so the
//! oplog audit trail records who did what when.

use clap::{Args, Subcommand};

#[derive(Clone, Debug, Subcommand)]
pub enum RedactCommands {
    /// Declare a redaction on a blob in a state. The blob bytes stay
    /// on disk; reads return the stub. Use `heddle purge` afterward
    /// to physically remove the bytes.
    Apply(RedactApplyArgs),
    /// List every active redaction in the repo.
    List(RedactListArgs),
    /// Show a single redaction by its content-addressed id.
    Show(RedactShowArgs),
}

#[derive(Clone, Debug, Args)]
pub struct RedactApplyArgs {
    /// State that surfaces the file. Accepts short or full state IDs,
    /// marker names, `HEAD`, `@`, or `HEAD~N`.
    pub state: String,
    /// Path within the state's tree.
    #[arg(long)]
    pub path: String,
    /// Operator-supplied reason. Lands in the materialized stub so
    /// reviewers know why content disappeared.
    #[arg(long)]
    pub reason: String,
    /// Walk every reachable state and redact every occurrence of the
    /// same blob hash. Default: just the named state.
    #[arg(long)]
    pub all_states: bool,
    /// Path to a private key (PEM) used to sign the redaction. The
    /// signature binds operator → declaration; auditors can verify
    /// who hid what when with `heddle redact show`.
    #[arg(long, value_name = "PATH")]
    pub sign_with: Option<std::path::PathBuf>,
    /// Override the signing algorithm. Defaults to autodetect from the
    /// key file's PEM header. Accepts `ed25519`, `rsa`, `p256`.
    #[arg(long, value_name = "ALGO", requires = "sign_with")]
    pub sign_algo: Option<String>,
}

#[derive(Clone, Debug, Args)]
pub struct RedactListArgs {}

#[derive(Clone, Debug, Args)]
pub struct RedactShowArgs {
    /// Redaction id (full or short prefix).
    pub redaction_id: String,
}

#[derive(Clone, Debug, Subcommand)]
pub enum PurgeCommands {
    /// Physically remove the blob bytes referenced by an existing
    /// redaction. Refuses if no redaction declared the blob first.
    ///
    /// Workspace-owner capability today; the surface is documented in
    /// the build brief at `.agents/redaction-primitive.md`. The
    /// capability check is a TODO until Biscuit wiring lands; for now
    /// `--force` is the explicit confirmation step.
    Apply(PurgeApplyArgs),
    /// List every `Purge` oplog entry — who removed bytes, when, and
    /// which redaction the purge acted on.
    List(PurgeListArgs),
}

#[derive(Clone, Debug, Args)]
pub struct PurgeApplyArgs {
    /// State whose redaction we're purging the blob of.
    pub state: String,
    /// Path within the state's tree.
    #[arg(long)]
    pub path: String,
    /// Required acknowledgement. Purge is irreversible — without
    /// `--force` the command refuses, listing what would be removed.
    #[arg(long)]
    pub force: bool,
}

#[derive(Clone, Debug, Args)]
pub struct PurgeListArgs {}