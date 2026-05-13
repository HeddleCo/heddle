// SPDX-License-Identifier: Apache-2.0
//! Context annotation subcommands.

/// Context subcommands.
#[derive(Clone, Debug, clap::Subcommand)]
pub enum ContextCommands {
    /// Attach a context annotation to a file, symbol, line range, or state.
    Set(ContextSetArgs),

    /// Show current context annotations for a file or state target.
    Get(ContextGetArgs),

    /// List all active context targets.
    List(ContextListArgs),

    /// Show full revision history for one logical annotation.
    History(ContextHistoryArgs),

    /// Add a new revision to an existing logical annotation.
    Edit(ContextEditArgs),

    /// Create a replacement logical annotation and supersede an older one.
    Supersede(ContextSupersedeArgs),

    /// Remove context annotations.
    Rm(ContextRmArgs),

    /// Check annotation staleness against current code.
    Check(ContextCheckArgs),

    /// Suggest low-noise targets that may benefit from context.
    Suggest(ContextSuggestArgs),

    /// Audit stale, superseded, and duplicate context.
    Audit(ContextAuditArgs),
}

#[derive(Clone, Debug, clap::Args)]
pub struct ContextTargetArgs {
    /// File path to annotate/query.
    #[arg(long, conflicts_with = "state")]
    pub path: Option<String>,

    /// State/change ID for broader guidance.
    #[arg(long, conflicts_with = "path")]
    pub state: Option<String>,
}

/// Arguments for `heddle context set`.
#[derive(Clone, Debug, clap::Args)]
pub struct ContextSetArgs {
    #[command(flatten)]
    pub target: ContextTargetArgs,

    /// Annotation scope: "file" (default), "symbol:<name>", or "lines:<start>-<end>".
    #[arg(short, long)]
    pub scope: Option<String>,

    /// Primary annotation kind: constraint, invariant, or rationale.
    #[arg(long, default_value = "rationale")]
    pub kind: String,

    /// Explicit tags for categorization (can be repeated).
    #[arg(long)]
    pub tag: Vec<String>,

    /// Annotation content (inline).
    #[arg(short = 'm', long)]
    pub message: Option<String>,

    /// Read annotation content from a file.
    #[arg(long)]
    pub file: Option<std::path::PathBuf>,
}

/// Arguments for `heddle context get`.
#[derive(Clone, Debug, clap::Args)]
pub struct ContextGetArgs {
    #[command(flatten)]
    pub target: ContextTargetArgs,

    /// Filter by scope.
    #[arg(short, long)]
    pub scope: Option<String>,

    /// Filter by tag.
    #[arg(long)]
    pub tag: Option<String>,

    /// Read context from an explicit historical ref/state instead of HEAD.
    #[arg(long)]
    pub r#ref: Option<String>,
}

/// Arguments for `heddle context list`.
#[derive(Clone, Debug, clap::Args)]
pub struct ContextListArgs {
    /// Optional path prefix to filter file targets by.
    #[arg(long)]
    pub prefix: Option<String>,

    /// Filter by tag.
    #[arg(long)]
    pub tag: Option<String>,

    /// Read context from an explicit historical ref/state instead of HEAD.
    #[arg(long)]
    pub r#ref: Option<String>,

    /// Include superseded logical annotations in listings.
    #[arg(long)]
    pub include_superseded: bool,
}

#[derive(Clone, Debug, clap::Args)]
pub struct ContextHistoryArgs {
    /// Stable logical annotation ID.
    pub annotation_id: String,

    /// Read context from an explicit historical ref/state instead of HEAD.
    #[arg(long)]
    pub r#ref: Option<String>,
}

#[derive(Clone, Debug, clap::Args)]
pub struct ContextEditArgs {
    /// Stable logical annotation ID.
    pub annotation_id: String,

    /// Override the annotation kind for the new revision.
    #[arg(long)]
    pub kind: Option<String>,

    /// Explicit tags for the new revision (can be repeated).
    #[arg(long)]
    pub tag: Vec<String>,

    /// New revision content (inline).
    #[arg(short = 'm', long)]
    pub message: Option<String>,

    /// Read revision content from a file.
    #[arg(long)]
    pub file: Option<std::path::PathBuf>,
}

#[derive(Clone, Debug, clap::Args)]
pub struct ContextSupersedeArgs {
    /// Stable logical annotation ID to supersede.
    pub annotation_id: String,

    #[command(flatten)]
    pub target: ContextTargetArgs,

    /// Replacement annotation scope.
    #[arg(short, long)]
    pub scope: Option<String>,

    /// Replacement annotation kind: constraint, invariant, or rationale.
    #[arg(long, default_value = "rationale")]
    pub kind: String,

    /// Explicit tags for the replacement annotation.
    #[arg(long)]
    pub tag: Vec<String>,

    /// Replacement annotation content (inline).
    #[arg(short = 'm', long)]
    pub message: Option<String>,

    /// Read replacement content from a file.
    #[arg(long)]
    pub file: Option<std::path::PathBuf>,
}

/// Arguments for `heddle context rm`.
#[derive(Clone, Debug, clap::Args)]
pub struct ContextRmArgs {
    #[command(flatten)]
    pub target: ContextTargetArgs,

    /// Remove only annotations matching this scope.
    #[arg(short, long)]
    pub scope: Option<String>,

    /// Remove all annotations for this target.
    #[arg(long)]
    pub all: bool,
}

/// Arguments for `heddle context check`.
#[derive(Clone, Debug, clap::Args)]
pub struct ContextCheckArgs {
    /// File path to check (checks all annotated files if omitted).
    #[arg(long)]
    pub path: Option<String>,

    /// State ID to check broader guidance on.
    #[arg(long)]
    pub state: Option<String>,

    /// Filter by tag.
    #[arg(long)]
    pub tag: Option<String>,

    /// Read context from an explicit historical ref/state instead of HEAD.
    #[arg(long)]
    pub r#ref: Option<String>,
}

#[derive(Clone, Debug, clap::Args)]
pub struct ContextSuggestArgs {
    /// Read suggestions from an explicit historical ref/state instead of HEAD.
    #[arg(long)]
    pub r#ref: Option<String>,

    /// Maximum suggestions to print.
    #[arg(short = 'n', long, default_value = "10")]
    pub limit: usize,
}

#[derive(Clone, Debug, clap::Args)]
pub struct ContextAuditArgs {
    /// Read context from an explicit historical ref/state instead of HEAD.
    #[arg(long)]
    pub r#ref: Option<String>,
}