// SPDX-License-Identifier: Apache-2.0
//! `heddle discuss` — durable repository collaboration.

use clap::{ArgGroup, Args, Subcommand};

#[derive(Clone, Debug, Subcommand)]
pub enum DiscussCommands {
    /// Open a discussion anchored to a symbol.
    Open(DiscussOpenArgs),
    /// Add a durable turn to a discussion.
    Turn(DiscussTurnArgs),
    /// Resolve a discussion.
    Resolve(DiscussResolveArgs),
    /// Reopen a resolved discussion.
    Reopen(DiscussReopenArgs),
    /// List repository discussions.
    List(DiscussListArgs),
    /// Show one discussion and its causal heads.
    Show(DiscussShowArgs),
    /// Replay hosted discussion events after the local watermark, then go live.
    Wait(DiscussWaitArgs),
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
pub struct DiscussTurnArgs {
    /// Discussion id, or FILE when copying `discuss open` argv.
    #[arg(value_name = "ID|FILE")]
    pub discussion_id: String,
    /// Turn body, or SYMBOL when copying `discuss open` argv.
    #[arg(value_name = "BODY|SYMBOL")]
    pub body: String,
    /// Turn body when using `FILE SYMBOL BODY`.
    #[arg(value_name = "BODY")]
    pub open_body: Option<String>,
}

#[derive(Clone, Debug, Args)]
#[command(group(
    ArgGroup::new("resolution")
        .required(true)
        .args(["mode", "into_annotation", "dismiss", "by_edit"])
))]
pub struct DiscussResolveArgs {
    /// Discussion id, or FILE when copying `discuss open` argv.
    #[arg(value_name = "ID|FILE")]
    pub discussion_id: String,
    /// SYMBOL when resolving with `FILE SYMBOL` instead of an id.
    #[arg(value_name = "SYMBOL")]
    pub symbol: Option<String>,
    /// Resolution kind: `by-edit` or `dismiss`.
    #[arg(long, value_enum)]
    pub mode: Option<ResolveModeArg>,
    /// Shorthand for `--mode dismiss`. Still requires `--reason`.
    #[arg(long, conflicts_with_all = ["mode", "by_edit", "into_annotation"])]
    pub dismiss: bool,
    /// Shorthand for `--mode by-edit`.
    #[arg(long = "by-edit", conflicts_with_all = ["mode", "dismiss", "into_annotation"])]
    pub by_edit: bool,
    /// Resolve by creating a real context annotation (`context set`) and linking it.
    #[arg(long, requires = "body")]
    pub into_annotation: bool,
    /// For `by-edit`: state containing the edit (defaults to HEAD).
    #[arg(long)]
    pub state: Option<String>,
    /// For `dismiss`: non-empty reason.
    #[arg(long)]
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

impl DiscussResolveArgs {
    /// Effective `--mode`, including `--dismiss` / `--by-edit` shorthands.
    pub fn resolved_mode(&self) -> Option<ResolveModeArg> {
        if self.dismiss {
            Some(ResolveModeArg::Dismiss)
        } else if self.by_edit {
            Some(ResolveModeArg::ByEdit)
        } else {
            self.mode.clone()
        }
    }
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
    /// Discussion id, or FILE when copying `discuss open` argv.
    #[arg(value_name = "ID|FILE")]
    pub discussion_id: String,
    /// SYMBOL when showing with `FILE SYMBOL` instead of an id.
    #[arg(value_name = "SYMBOL")]
    pub symbol: Option<String>,
}

#[derive(Clone, Debug, Args)]
pub struct DiscussWaitArgs {
    /// Resume after this hosted event id. Defaults to the persisted watermark.
    #[arg(long)]
    pub after: Option<i64>,
    /// Hosted remote that owns the event cursor. Defaults to the repository default.
    #[arg(long)]
    pub remote: Option<String>,
    /// Restrict the subscription to this thread name.
    #[arg(long)]
    pub thread: Option<String>,
    /// Internal helper for tests: stop after this many events (including ignored ones).
    #[arg(long, hide = true)]
    pub max_events: Option<usize>,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::cli::{Cli, Commands, DiscussCommands, ResolveModeArg};

    #[test]
    fn turn_accepts_id_or_open_argv() {
        match Cli::try_parse_from(["heddle", "discuss", "turn", "disc-id", "body"])
            .expect("id argv")
            .command
        {
            Commands::Discuss {
                command: DiscussCommands::Turn(args),
            } => {
                assert_eq!(args.discussion_id, "disc-id");
                assert_eq!(args.body, "body");
                assert!(args.open_body.is_none());
            }
            _ => panic!("expected discuss turn"),
        }
        match Cli::try_parse_from([
            "heddle",
            "discuss",
            "turn",
            "src/auth.rs",
            "verify",
            "body",
        ])
        .expect("open argv")
        .command
        {
            Commands::Discuss {
                command: DiscussCommands::Turn(args),
            } => {
                assert_eq!(args.discussion_id, "src/auth.rs");
                assert_eq!(args.body, "verify");
                assert_eq!(args.open_body.as_deref(), Some("body"));
            }
            _ => panic!("expected discuss turn"),
        }
    }

    #[test]
    fn show_and_resolve_accept_file_symbol() {
        match Cli::try_parse_from(["heddle", "discuss", "show", "src/auth.rs", "verify"])
            .expect("show open argv")
            .command
        {
            Commands::Discuss {
                command: DiscussCommands::Show(args),
            } => {
                assert_eq!(args.discussion_id, "src/auth.rs");
                assert_eq!(args.symbol.as_deref(), Some("verify"));
            }
            _ => panic!("expected discuss show"),
        }
        match Cli::try_parse_from([
            "heddle",
            "discuss",
            "resolve",
            "src/auth.rs",
            "verify",
            "--mode",
            "dismiss",
            "--reason",
            "done",
        ])
        .expect("resolve open argv")
        .command
        {
            Commands::Discuss {
                command: DiscussCommands::Resolve(args),
            } => {
                assert_eq!(args.discussion_id, "src/auth.rs");
                assert_eq!(args.symbol.as_deref(), Some("verify"));
            }
            _ => panic!("expected discuss resolve"),
        }
    }

    #[test]
    fn resolve_dismiss_and_by_edit_shorthands_set_mode() {
        match Cli::try_parse_from([
            "heddle",
            "discuss",
            "resolve",
            "disc-id",
            "--dismiss",
            "--reason",
            "done",
        ])
        .expect("dismiss shorthand")
        .command
        {
            Commands::Discuss {
                command: DiscussCommands::Resolve(args),
            } => {
                assert!(matches!(args.resolved_mode(), Some(ResolveModeArg::Dismiss)));
                assert_eq!(args.reason.as_deref(), Some("done"));
            }
            _ => panic!("expected discuss resolve"),
        }
        match Cli::try_parse_from([
            "heddle",
            "discuss",
            "resolve",
            "disc-id",
            "--by-edit",
        ])
        .expect("by-edit shorthand")
        .command
        {
            Commands::Discuss {
                command: DiscussCommands::Resolve(args),
            } => {
                assert!(matches!(args.resolved_mode(), Some(ResolveModeArg::ByEdit)));
            }
            _ => panic!("expected discuss resolve"),
        }
    }

}
