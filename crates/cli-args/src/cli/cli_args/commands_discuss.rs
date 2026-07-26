// SPDX-License-Identifier: Apache-2.0
//! `heddle discuss` — durable repository collaboration.

use clap::{Args, Subcommand};

#[derive(Clone, Debug, Subcommand)]
pub enum DiscussCommands {
    /// Open a discussion anchored to a symbol.
    Open(DiscussOpenArgs),
    /// Append a durable turn to a discussion.
    Append(DiscussAppendArgs),
    /// Resolve a discussion.
    Resolve(DiscussResolveArgs),
    /// Reopen a resolved discussion.
    Reopen(DiscussReopenArgs),
    /// List repository discussions.
    List(DiscussListArgs),
    /// Show one discussion and its causal heads.
    Show(DiscussShowArgs),
}

#[derive(Clone, Debug, Args)]
pub struct DiscussOpenArgs {
    /// Path of the file containing the symbol.
    pub file: String,
    /// Symbol name (for example `Repository::open`).
    pub symbol: String,
    /// First turn of the discussion.
    pub body: String,
    /// Human-readable summary. Defaults to the first line of the first turn.
    #[arg(long)]
    pub title: Option<String>,
    /// State the symbol anchor was observed against. Defaults to HEAD.
    #[arg(long)]
    pub state: Option<String>,
    /// Visibility: `public` | `internal` | `team:NAME` | `restricted:LABEL` | `private:LABEL`.
    #[arg(long)]
    pub visibility: Option<String>,
}

#[derive(Clone, Debug, Args)]
pub struct DiscussAppendArgs {
    pub discussion_id: String,
    pub body: String,
}

#[derive(Clone, Debug, Args)]
pub struct DiscussResolveArgs {
    pub discussion_id: String,
    /// Resolution kind: `by-edit` or `dismiss`.
    #[arg(long, value_enum)]
    pub mode: ResolveModeArg,
    /// For `by-edit`: state containing the edit (defaults to HEAD).
    #[arg(long)]
    pub state: Option<String>,
    /// For `dismiss`: non-empty reason.
    #[arg(long)]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, clap::ValueEnum)]
pub enum ResolveModeArg {
    ByEdit,
    Dismiss,
}

#[derive(Clone, Debug, Args)]
pub struct DiscussReopenArgs {
    pub discussion_id: String,
    /// Why the prior resolution no longer applies.
    #[arg(long)]
    pub reason: String,
}

#[derive(Clone, Debug, Args)]
pub struct DiscussListArgs {
    /// Filter by the state named in the discussion anchor.
    #[arg(long)]
    pub state: Option<String>,
    /// Filter by anchored file path.
    #[arg(long)]
    pub file: Option<String>,
    /// Filter by anchored symbol. Requires `--file`.
    #[arg(long)]
    pub symbol: Option<String>,
    /// Status filter: `open`, `resolved`, `conflicted`, or `all`.
    #[arg(long, default_value = "open")]
    pub status: String,
}

#[derive(Clone, Debug, Args)]
pub struct DiscussShowArgs {
    pub discussion_id: String,
}
