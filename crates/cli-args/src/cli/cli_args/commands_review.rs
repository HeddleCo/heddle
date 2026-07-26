// SPDX-License-Identifier: Apache-2.0
//! `heddle review` — internal state review surface (R7).

use clap::{Args, Subcommand, ValueEnum};

#[derive(Clone, Debug, Subcommand)]
pub enum ReviewCommands {
    /// Render the review payload for a state.
    Show(ReviewShowArgs),
    /// Submit a review signature on a state.
    Sign(ReviewSignArgs),
    /// Walk to the next pending review when review selection is configured.
    Next(ReviewNextArgs),
    /// Per-module signal health over a rolling window.
    Health(ReviewHealthArgs),
}

#[derive(Clone, Debug, Args)]
pub struct ReviewShowArgs {
    /// State to review. Defaults to HEAD.
    pub state: Option<String>,
    /// Include hidden signals beyond the in-budget set.
    #[arg(long)]
    pub all_signals: bool,
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

    pub fn as_proto(&self) -> api::heddle::api::v1alpha1::ReviewKind {
        use api::heddle::api::v1alpha1::ReviewKind;
        match self {
            Self::Read => ReviewKind::Read,
            Self::AgentPreview => ReviewKind::AgentPreview,
            Self::AgentCoReview => ReviewKind::AgentCoReview,
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
