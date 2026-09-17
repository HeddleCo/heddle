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

    /// Mine external sources for context annotations.
    #[cfg(all(feature = "git-overlay", feature = "ingest"))]
    Reason {
        #[command(subcommand)]
        command: ContextReasonCommands,
    },
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

#[cfg(all(feature = "git-overlay", feature = "ingest"))]
#[derive(Clone, Debug, clap::Subcommand)]
pub enum ContextReasonCommands {
    /// Mine Git-agent transcripts and attach reasoning as context annotations.
    Git(ContextReasonGitArgs),
}

#[cfg(all(feature = "git-overlay", feature = "ingest"))]
#[derive(Clone, Debug, clap::Args)]
pub struct ContextReasonGitArgs {
    /// Source git repository the transcripts are about.
    #[arg(long)]
    pub path: std::path::PathBuf,

    /// Cap candidates per commit. Higher = more coverage at the cost of cross-attribution.
    #[arg(long, default_value_t = 5)]
    pub max_sessions_per_commit: usize,

    /// Drop sessions below this confidence.
    #[arg(long, default_value_t = 0.20)]
    pub min_match_confidence: f32,

    /// Limit how many commits the reason pass walks.
    #[arg(long)]
    pub limit: Option<usize>,

    /// Override the Claude transcript store. Empty string disables.
    #[arg(long = "claude-home")]
    pub claude_home: Option<String>,

    /// Override the Codex transcript store. Empty string disables.
    #[arg(long = "codex-home")]
    pub codex_home: Option<String>,

    /// Override the OpenCode data dir. Empty string disables.
    #[arg(long = "opencode-home")]
    pub opencode_home: Option<String>,

    /// Do not write annotations; only report what would happen.
    #[arg(long)]
    pub dry_run: bool,
}

/// Arguments for `heddle context set`.
#[derive(Clone, Debug, clap::Args)]
pub struct ContextSetArgs {
    /// File path to annotate (alternative to `--path`).
    #[arg(value_name = "PATH", conflicts_with_all = ["path", "state"])]
    pub path_positional: Option<String>,

    #[command(flatten)]
    pub target: ContextTargetArgs,

    /// Annotation scope: "file" (default), "symbol:<name>", or "lines:<start>-<end>".
    #[arg(short, long)]
    pub scope: Option<String>,

    /// Primary annotation kind: constraint, invariant, or rationale.
    #[arg(
        long,
        default_value = "rationale",
        value_parser = ["constraint", "invariant", "rationale"]
    )]
    pub kind: String,

    /// Explicit tags for categorization (can be repeated).
    #[arg(long)]
    pub tag: Vec<String>,

    /// Annotation content (inline). `--body` is an alias of `--message`/`-m`.
    #[arg(short = 'm', long, visible_alias = "body")]
    pub message: Option<String>,

    /// Read annotation content from a file.
    #[arg(long = "from-file", value_name = "PATH")]
    pub from_file: Option<std::path::PathBuf>,
}

impl ContextSetArgs {
    /// Effective file path from positional PATH or `--path`.
    pub fn resolved_path(&self) -> Option<&str> {
        self.path_positional
            .as_deref()
            .or(self.target.path.as_deref())
    }
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
    /// Stable logical annotation ID. Omit when using `--path` / `--state`.
    #[arg(required_unless_present_any = ["path", "state"])]
    pub annotation_id: Option<String>,

    #[command(flatten)]
    pub target: ContextTargetArgs,

    /// Read context from an explicit historical ref/state instead of HEAD.
    #[arg(long)]
    pub r#ref: Option<String>,
}

#[derive(Clone, Debug, clap::Args)]
pub struct ContextEditArgs {
    /// Stable logical annotation ID. Omit when using `--path` / `--state`.
    #[arg(required_unless_present_any = ["path", "state"])]
    pub annotation_id: Option<String>,

    #[command(flatten)]
    pub target: ContextTargetArgs,

    /// Override the annotation kind for the new revision.
    #[arg(long, value_parser = ["constraint", "invariant", "rationale"])]
    pub kind: Option<String>,

    /// Explicit tags for the new revision (can be repeated).
    #[arg(long)]
    pub tag: Vec<String>,

    /// New revision content (inline). `--body` is an alias of `--message`/`-m`.
    #[arg(short = 'm', long, visible_alias = "body")]
    pub message: Option<String>,

    /// Read revision content from a file.
    #[arg(long = "from-file", value_name = "PATH")]
    pub from_file: Option<std::path::PathBuf>,
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
    #[arg(
        long,
        default_value = "rationale",
        value_parser = ["constraint", "invariant", "rationale"]
    )]
    pub kind: String,

    /// Explicit tags for the replacement annotation.
    #[arg(long)]
    pub tag: Vec<String>,

    /// Replacement annotation content (inline). `--body` is an alias of `--message`/`-m`.
    #[arg(short = 'm', long, visible_alias = "body")]
    pub message: Option<String>,

    /// Read replacement content from a file.
    #[arg(long = "from-file", value_name = "PATH")]
    pub from_file: Option<std::path::PathBuf>,
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

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::cli::{Cli, Commands, ContextCommands};

    #[test]
    fn history_and_edit_accept_path_or_id() {
        match Cli::try_parse_from(["heddle", "context", "history", "ann-1"])
            .expect("history id")
            .command
        {
            Commands::Context {
                command: ContextCommands::History(args),
            } => {
                assert_eq!(args.annotation_id.as_deref(), Some("ann-1"));
                assert!(args.target.path.is_none());
            }
            _ => panic!("expected context history"),
        }
        match Cli::try_parse_from(["heddle", "context", "history", "--path", "src/auth.rs"])
            .expect("history path")
            .command
        {
            Commands::Context {
                command: ContextCommands::History(args),
            } => {
                assert!(args.annotation_id.is_none());
                assert_eq!(args.target.path.as_deref(), Some("src/auth.rs"));
            }
            _ => panic!("expected context history"),
        }
        match Cli::try_parse_from([
            "heddle",
            "context",
            "edit",
            "--path",
            "src/auth.rs",
            "-m",
            "revised",
        ])
        .expect("edit path")
        .command
        {
            Commands::Context {
                command: ContextCommands::Edit(args),
            } => {
                assert!(args.annotation_id.is_none());
                assert_eq!(args.target.path.as_deref(), Some("src/auth.rs"));
                assert_eq!(args.message.as_deref(), Some("revised"));
            }
            _ => panic!("expected context edit"),
        }
    }

    #[test]
    fn context_set_accepts_positional_path_body_alias_and_from_file() {
        match Cli::try_parse_from([
            "heddle",
            "context",
            "set",
            "src/auth.rs",
            "--body",
            "keep timing constant",
        ])
        .expect("positional path + body")
        .command
        {
            Commands::Context {
                command: ContextCommands::Set(args),
            } => {
                assert_eq!(args.resolved_path(), Some("src/auth.rs"));
                assert_eq!(args.message.as_deref(), Some("keep timing constant"));
            }
            _ => panic!("expected context set"),
        }
        match Cli::try_parse_from([
            "heddle",
            "context",
            "set",
            "--path",
            "src/auth.rs",
            "--from-file",
            "note.md",
        ])
        .expect("from-file")
        .command
        {
            Commands::Context {
                command: ContextCommands::Set(args),
            } => {
                assert_eq!(args.resolved_path(), Some("src/auth.rs"));
                assert_eq!(
                    args.from_file.as_deref(),
                    Some(std::path::Path::new("note.md"))
                );
            }
            _ => panic!("expected context set"),
        }
        assert!(
            Cli::try_parse_from(["heddle", "context", "set", "--file", "note.md"]).is_err(),
            "old --file content flag must not parse"
        );
    }
}
