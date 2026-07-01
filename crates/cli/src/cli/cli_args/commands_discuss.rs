// SPDX-License-Identifier: Apache-2.0
//! `heddle discuss` — anchored discussions on symbols.

use clap::{Args, Subcommand};

#[derive(Clone, Debug, Subcommand)]
pub enum DiscussCommands {
    /// Open a new discussion anchored to a symbol.
    Open(DiscussOpenArgs),
    /// Append a turn to an existing discussion.
    Append(DiscussAppendArgs),
    /// Resolve a discussion (by-edit or dismissed; into-annotation is not wired yet).
    Resolve(DiscussResolveArgs),
    /// List discussions on a state, symbol, or by status.
    List(DiscussListArgs),
    /// Show a single discussion.
    Show(DiscussShowArgs),
}

#[derive(Clone, Debug, Args)]
pub struct DiscussOpenArgs {
    /// Path of the file containing the symbol.
    pub file: String,
    /// Symbol name (e.g. `Repository::open`).
    pub symbol: String,
    /// First turn of the discussion.
    pub body: String,
    /// State the discussion anchors against. Defaults to HEAD.
    #[arg(long)]
    pub state: Option<String>,
    /// Visibility: `public` | `internal` | `team:NAME` | `restricted:LABEL` | `private:LABEL`.
    #[arg(long)]
    pub visibility: Option<String>,
    /// Optional thread reference for grouping.
    #[arg(long)]
    pub thread: Option<String>,
}

#[derive(Clone, Debug, Args)]
pub struct DiscussAppendArgs {
    pub discussion_id: String,
    pub body: String,
}

#[derive(Clone, Debug, Args)]
pub struct DiscussResolveArgs {
    pub discussion_id: String,
    /// Resolution kind: `by-edit` | `dismiss`. `into-annotation` is reserved but unavailable.
    #[arg(long, value_enum)]
    pub mode: ResolveModeArg,
    /// Reserved for future `into-annotation`: annotation kind.
    #[arg(long)]
    pub annotation_kind: Option<String>,
    /// Reserved for future `into-annotation`: annotation content.
    #[arg(long)]
    pub annotation_content: Option<String>,
    /// Reserved for future `into-annotation`: optional comma-separated tags.
    #[arg(long)]
    pub annotation_tags: Option<String>,
    /// For `by-edit`: state the edit lives on (defaults to HEAD).
    #[arg(long)]
    pub state: Option<String>,
    /// For `dismiss`: reason (required).
    #[arg(long)]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, clap::ValueEnum)]
pub enum ResolveModeArg {
    IntoAnnotation,
    ByEdit,
    Dismiss,
}

#[derive(Clone, Debug, Args)]
pub struct DiscussListArgs {
    /// Filter by state. When omitted, lists discussions on HEAD.
    #[arg(long)]
    pub state: Option<String>,
    /// Filter by file path.
    #[arg(long)]
    pub file: Option<String>,
    /// Filter by symbol name. Requires `--file`; repository-wide symbol lookup is not wired yet.
    #[arg(long)]
    pub symbol: Option<String>,
    /// Status filter: `open`|`resolved`|`all`|`orphaned`. Default `all`.
    #[arg(long, default_value = "all")]
    pub status: String,
}

#[derive(Clone, Debug, Args)]
pub struct DiscussShowArgs {
    pub discussion_id: String,
    /// Resolve the discussion against this state instead of HEAD. Use when a
    /// discussion lives on a prior state (found via `discuss list --state <s>`)
    /// and is no longer on HEAD.
    #[arg(long)]
    pub state: Option<String>,
}
