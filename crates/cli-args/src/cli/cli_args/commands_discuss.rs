// SPDX-License-Identifier: Apache-2.0
//! `heddle discuss` — durable repository collaboration.

use clap::{ArgGroup, Args, Subcommand};

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
#[command(
    group(
        ArgGroup::new("positional_open")
            .args(["file", "symbol", "body"])
            .multiple(true)
            .conflicts_with("named_open")
    ),
    group(
        ArgGroup::new("named_open")
            .args(["file_flag", "symbol_flag", "body_flag"])
            .multiple(true)
    )
)]
pub struct DiscussOpenArgs {
    /// Path of the file containing the symbol.
    #[arg(
        value_name = "FILE",
        required_unless_present_all = ["file_flag", "symbol_flag", "body_flag"]
    )]
    pub file: Option<String>,
    /// Symbol name (for example `Repository::open`).
    #[arg(
        value_name = "SYMBOL",
        required_unless_present_all = ["file_flag", "symbol_flag", "body_flag"]
    )]
    pub symbol: Option<String>,
    /// First turn of the discussion.
    #[arg(
        value_name = "BODY",
        required_unless_present_all = ["file_flag", "symbol_flag", "body_flag"]
    )]
    pub body: Option<String>,
    /// Path of the file containing the symbol (named alternative to `<FILE>`).
    #[arg(
        long = "file",
        value_name = "FILE",
        required_unless_present_all = ["file", "symbol", "body"]
    )]
    pub file_flag: Option<String>,
    /// Symbol name (named alternative to `<SYMBOL>`).
    #[arg(
        long = "symbol",
        value_name = "SYMBOL",
        required_unless_present_all = ["file", "symbol", "body"]
    )]
    pub symbol_flag: Option<String>,
    /// First turn (named alternative to `<BODY>`).
    #[arg(
        long = "body",
        value_name = "BODY",
        required_unless_present_all = ["file", "symbol", "body"]
    )]
    pub body_flag: Option<String>,
    /// Human-readable summary. Defaults to the first line of the first turn.
    #[arg(long)]
    pub title: Option<String>,
    /// State the symbol anchor was observed against. Defaults to HEAD.
    #[arg(long)]
    pub state: Option<String>,
    /// Visibility: `public` | `internal` | `team:NAME` | `restricted:LABEL` | `private:LABEL`.
    #[arg(long)]
    pub visibility: Option<String>,
    /// Attach the discussion to a thread ref while keeping its symbol anchor.
    #[arg(long, value_name = "REF")]
    pub thread: Option<String>,
}

#[derive(Clone, Debug, Args)]
pub struct DiscussAppendArgs {
    pub discussion_id: String,
    pub body: String,
}

#[derive(Clone, Debug, Args)]
#[command(group(
    ArgGroup::new("resolution")
        .required(true)
        .args(["mode", "into_annotation"])
))]
pub struct DiscussResolveArgs {
    pub discussion_id: String,
    /// Resolution kind: `by-edit` or `dismiss`.
    #[arg(long, value_enum)]
    pub mode: Option<ResolveModeArg>,
    /// Resolve by creating a context annotation from this discussion.
    #[arg(long, requires = "body")]
    pub into_annotation: bool,
    /// For `by-edit`: state containing the edit (defaults to HEAD).
    #[arg(long, requires = "mode")]
    pub state: Option<String>,
    /// For `dismiss`: non-empty reason.
    #[arg(long, requires = "mode")]
    pub reason: Option<String>,
    /// For `--into-annotation`: annotation content.
    #[arg(long, requires = "into_annotation")]
    pub body: Option<String>,
    /// For `--into-annotation`: constraint, invariant, or rationale (defaults to rationale).
    #[arg(
        long,
        value_parser = ["constraint", "invariant", "rationale"],
        requires = "into_annotation"
    )]
    pub kind: Option<String>,
    /// For `--into-annotation`: annotation tag (can be repeated).
    #[arg(long, requires = "into_annotation")]
    pub tag: Vec<String>,
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
