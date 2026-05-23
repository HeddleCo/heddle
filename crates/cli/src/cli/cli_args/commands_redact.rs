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
    /// Manage the list of operator public keys whose signed
    /// redactions this repo accepts over the wire.
    ///
    /// `accept_wire_redactions` is fail-closed: an empty trust list
    /// rejects every signed redaction. Operators run `heddle redact
    /// trust add` to authorize a key for cross-replica propagation.
    /// Signing alone proves *who* declared the redaction; the trust
    /// list proves the receiver has authorized that operator to act
    /// on this workspace.
    #[command(subcommand)]
    Trust(RedactTrustCommands),
}

#[derive(Clone, Debug, Subcommand)]
pub enum RedactTrustCommands {
    /// Add an operator public key to `[redact] trusted_keys` in
    /// `.heddle/config.toml`. Subsequent `heddle fetch`/`clone`
    /// invocations will accept signed redactions from that key.
    Add(RedactTrustAddArgs),
    /// List the currently-trusted operator keys.
    List(RedactTrustListArgs),
    /// Remove an operator public key from the trust list. Future
    /// signed redactions from that key will be refused.
    Remove(RedactTrustRemoveArgs),
}

#[derive(Clone, Debug, Args)]
pub struct RedactTrustAddArgs {
    /// Path to a PEM file (public or private key — the public half
    /// is extracted either way). Algorithm autodetected from the PEM
    /// header. This is the operator-friendly path: same PEM you
    /// passed to `heddle redact apply --sign-with`.
    #[arg(long, value_name = "PATH", group = "key_source")]
    pub from_pem: Option<std::path::PathBuf>,
    /// Algorithm identifier (`ed25519`, `rsa`, `p256`) when supplying
    /// the raw hex-encoded public key directly via `--public-key`.
    #[arg(long, value_name = "ALGO", requires = "public_key")]
    pub algorithm: Option<String>,
    /// Hex-encoded raw public key bytes. Use alongside `--algorithm`
    /// when the operator already has the key in hex form (e.g. from
    /// a signed-redaction's `signature.public_key` field).
    #[arg(long, value_name = "HEX", requires = "algorithm", group = "key_source")]
    pub public_key: Option<String>,
    /// Optional free-text label for the trust entry (`"luke-laptop"`,
    /// `"ci-signing"`). Doesn't affect matching semantics.
    #[arg(long)]
    pub label: Option<String>,
}

#[derive(Clone, Debug, Args)]
pub struct RedactTrustListArgs {}

#[derive(Clone, Debug, Args)]
pub struct RedactTrustRemoveArgs {
    /// Hex-encoded raw public key to remove. Exact match (case-
    /// insensitive).
    pub public_key: String,
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
