// SPDX-License-Identifier: Apache-2.0
//! `heddle review` — signed Thread comparison review workflow.

use clap::{Args, Subcommand, ValueEnum};

use super::RemoteChoiceArgs;

#[derive(Clone, Debug, Subcommand)]
pub enum ReviewCommands {
    /// Show the exact source and target comparison for a Thread.
    Show(ReviewShowArgs),
    /// Approve the exact source and target comparison shown for this thread, using your configured signing identity.
    #[command(
        about = "Approve the exact source and target comparison shown for this thread, using your configured signing identity."
    )]
    Approve(ReviewApproveArgs),
    /// List signed review decisions for a Thread.
    List(ReviewListArgs),
    /// Revoke one of your signed approvals.
    Revoke(ReviewRevokeArgs),
    /// Check whether a Thread's exact comparison is ready to land into a target.
    Readiness(ReviewReadinessArgs),
    /// Submit a low-level review signature on a state.
    #[command(hide = true)]
    Sign(ReviewSignArgs),
    /// Walk to the next pending review when review selection is configured.
    #[command(hide = true)]
    Next(ReviewNextArgs),
    /// Per-module signal health over a rolling window.
    #[command(hide = true)]
    Health(ReviewHealthArgs),
}

#[derive(Clone, Debug, Args)]
pub struct ReviewShowArgs {
    /// Thread to review. Defaults to the current Thread.
    pub thread: Option<String>,
    #[command(flatten)]
    pub remote_choice: RemoteChoiceArgs,
}

#[derive(Clone, Debug, Args)]
pub struct ReviewApproveArgs {
    /// Thread to approve. Defaults to the current Thread.
    pub thread: Option<String>,
    /// Human note attached to the approval.
    #[arg(short = 'm', long)]
    pub message: Option<String>,
    #[command(flatten)]
    pub remote_choice: RemoteChoiceArgs,
}

#[derive(Clone, Debug, Args)]
pub struct ReviewListArgs {
    /// Thread whose review decisions to list. Defaults to the current Thread.
    pub thread: Option<String>,
    #[command(flatten)]
    pub remote_choice: RemoteChoiceArgs,
}

#[derive(Clone, Debug, Args)]
pub struct ReviewRevokeArgs {
    /// UUID of the approval to revoke.
    #[arg(value_name = "REVIEW_ID")]
    pub review_id: String,
    /// Thread containing the approval.
    #[arg(long, value_name = "THREAD")]
    pub thread: String,
    #[command(flatten)]
    pub remote_choice: RemoteChoiceArgs,
}

#[derive(Clone, Debug, Args)]
pub struct ReviewReadinessArgs {
    /// Source Thread to assess. Defaults to the current Thread.
    pub thread: Option<String>,
    /// Target Thread that would receive the source.
    #[arg(long, value_name = "TARGET")]
    pub into: String,
    #[command(flatten)]
    pub remote_choice: RemoteChoiceArgs,
}

#[derive(Clone, Debug, Args)]
pub struct ReviewSignArgs {
    pub state: String,
    /// Review kind.
    #[arg(long, value_enum)]
    pub kind: SignKindArg,
    /// Optional justification (unused for read/preview/co-review).
    #[arg(long)]
    pub justification: Option<String>,
    /// Optional symbol-level scope. Format: `file:symbol`. Repeat for
    /// multiple. Without any, the signature covers the whole change.
    #[arg(long)]
    pub symbols: Vec<String>,
    /// Cryptographic algorithm. Defaults to `ed25519`.
    #[arg(long, default_value = "ed25519")]
    pub algorithm: String,
    /// Public key in hex. Required.
    #[arg(long)]
    pub public_key: String,
    /// Signature bytes in hex. Required.
    #[arg(long)]
    pub signature: String,
    /// Unix timestamp (seconds) the client signed at. Required — the
    /// server verifies the signature over this exact timestamp and rejects
    /// values outside a small skew window.
    #[arg(long)]
    pub signed_at_unix: i64,
}

#[derive(Clone, Debug, ValueEnum)]
pub enum SignKindArg {
    Read,
    AgentPreview,
    AgentCoReview,
}

impl SignKindArg {
    pub fn as_wire(&self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::AgentPreview => "agent_preview",
            Self::AgentCoReview => "agent_co_review",
        }
    }

    pub fn as_proto(&self) -> objects::object::ReviewKind {
        match self {
            Self::Read => objects::object::ReviewKind::Read,
            Self::AgentPreview => objects::object::ReviewKind::AgentPreview,
            Self::AgentCoReview => objects::object::ReviewKind::AgentCoReview,
        }
    }
}

#[derive(Clone, Debug, Args)]
pub struct ReviewNextArgs {
    /// Only show reviews assigned to the current actor.
    #[arg(long)]
    pub mine_only: bool,
    /// Filter by review kind.
    #[arg(long)]
    pub kind: Option<String>,
}

#[derive(Clone, Debug, Args)]
pub struct ReviewHealthArgs {
    /// Number of recent states to consider. Server clamps to a sensible
    /// default when unset.
    #[arg(long)]
    pub window: Option<u32>,
}
