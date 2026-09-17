// SPDX-License-Identifier: Apache-2.0
//! `heddle discuss` — durable repository collaboration.

use std::path::PathBuf;

use clap::{Args, Subcommand};

/// Write flags plus the remaining discuss subcommands.
///
/// `heddle discuss --new …` / `heddle discuss --id …` is the write path.
/// `list` / `show` / `resolve` / `reopen` / `wait` stay subcommands.
#[derive(Clone, Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct DiscussArgs {
    /// Open a new discussion. Exclusive with `--id`.
    #[arg(long, conflicts_with = "id")]
    pub new: bool,
    /// Reply to an existing discussion (short or full `disc-` id). Exclusive with `--new`.
    #[arg(long, conflicts_with = "new")]
    pub id: Option<String>,
    /// Anchor file path.
    #[arg(long)]
    pub path: Option<String>,
    /// Anchor symbol. Requires `--path`.
    #[arg(long, requires = "path")]
    pub symbol: Option<String>,
    /// Anchor line (1-indexed). Requires `--path`.
    #[arg(long, requires = "path")]
    pub line: Option<u32>,
    /// Read the markdown body from a file.
    #[arg(long, value_name = "PATH")]
    pub file: Option<PathBuf>,
    /// Parent turn number (1-indexed). Requires `--id`. Defaults to the latest head.
    #[arg(long, requires = "id")]
    pub turn: Option<u32>,
    /// Human-readable summary. Defaults to the first line of the first turn.
    #[arg(long)]
    pub title: Option<String>,
    /// State the anchor was observed against. Defaults to HEAD.
    #[arg(long)]
    pub state: Option<String>,
    /// Visibility: `public` | `internal` | `team:NAME` | `restricted:LABEL` | `private:LABEL`.
    #[arg(long)]
    pub visibility: Option<String>,
    /// Attach the discussion to a thread ref while keeping its code anchor.
    #[arg(long, value_name = "REF")]
    pub thread: Option<String>,
    /// Turn body (markdown). Alternative to `--file`.
    #[arg(value_name = "BODY")]
    pub body: Option<String>,
    #[command(subcommand)]
    pub command: Option<DiscussCommands>,
}

#[derive(Clone, Debug, Subcommand)]
pub enum DiscussCommands {
    /// Open a discussion (hidden alias of `discuss --new`).
    #[command(hide = true)]
    Open(DiscussOpenArgs),
    /// Append a turn (hidden alias of `discuss --id`).
    #[command(hide = true)]
    Turn(DiscussTurnArgs),
    /// Removed: reply with `heddle discuss --id`.
    #[command(hide = true)]
    Append(DiscussAppendArgs),
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
pub struct DiscussOpenArgs {
    /// Legacy positional file path (`discuss open FILE SYMBOL BODY`).
    #[arg(value_name = "FILE")]
    pub positional_file: Option<String>,
    /// Legacy positional symbol.
    #[arg(value_name = "SYMBOL")]
    pub positional_symbol: Option<String>,
    /// First turn of the discussion.
    #[arg(value_name = "BODY")]
    pub body: Option<String>,
    /// Anchor file path.
    #[arg(long)]
    pub path: Option<String>,
    /// Anchor symbol.
    #[arg(long = "symbol")]
    pub symbol: Option<String>,
    /// Anchor line (1-indexed).
    #[arg(long)]
    pub line: Option<u32>,
    /// Read the markdown body from a file.
    #[arg(long = "file", value_name = "PATH")]
    pub file: Option<PathBuf>,
    /// First turn (named alternative to `<BODY>`).
    #[arg(long = "body")]
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
    /// Discussion id (short or full `disc-` id).
    #[arg(value_name = "ID")]
    pub discussion_id: String,
    /// Turn body.
    #[arg(value_name = "BODY")]
    pub body: String,
    /// Parent turn number (1-indexed). Defaults to the latest head.
    #[arg(long)]
    pub turn: Option<u32>,
}

#[derive(Clone, Debug, Args)]
pub struct DiscussAppendArgs {
    /// Discussion id, if the caller still passed one.
    #[arg(value_name = "ID")]
    pub discussion_id: Option<String>,
    /// Turn body, if the caller still passed one.
    #[arg(value_name = "BODY")]
    pub body: Option<String>,
}

#[derive(Clone, Debug, Args)]
#[command(group(
    clap::ArgGroup::new("resolution")
        .required(true)
        .args(["mode", "into_annotation", "dismiss", "by_edit"])
))]
pub struct DiscussResolveArgs {
    /// Discussion id (short or full `disc-` id).
    #[arg(value_name = "ID")]
    pub discussion_id: String,
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
    #[arg(long, alias = "file")]
    pub path: Option<String>,
    /// Filter by anchored symbol. Requires `--path`.
    #[arg(long, requires = "path")]
    pub symbol: Option<String>,
    /// Status filter: `open`, `resolved`, `conflicted`, or `all`.
    #[arg(long, default_value = "open")]
    pub status: String,
}

#[derive(Clone, Debug, Args)]
pub struct DiscussShowArgs {
    /// Discussion id (short or full `disc-` id).
    #[arg(value_name = "ID")]
    pub discussion_id: String,
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

    fn discuss(cli: Cli) -> crate::cli::DiscussArgs {
        match cli.command {
            Commands::Discuss(args) => args,
            _ => panic!("expected discuss"),
        }
    }

    #[test]
    fn write_path_parses_new_and_id() {
        let opened = discuss(
            Cli::try_parse_from([
                "heddle",
                "discuss",
                "--new",
                "--path",
                "src/lib.rs",
                "--symbol",
                "greet",
                "why greet?",
            ])
            .expect("discuss --new"),
        );
        assert!(opened.new);
        assert!(opened.id.is_none());
        assert_eq!(opened.path.as_deref(), Some("src/lib.rs"));
        assert_eq!(opened.symbol.as_deref(), Some("greet"));
        assert_eq!(opened.body.as_deref(), Some("why greet?"));
        assert!(opened.command.is_none());

        let reply = discuss(
            Cli::try_parse_from([
                "heddle",
                "discuss",
                "--id",
                "disc-01a0afc6",
                "second thought",
            ])
            .expect("discuss --id"),
        );
        assert!(!reply.new);
        assert_eq!(reply.id.as_deref(), Some("disc-01a0afc6"));
        assert_eq!(reply.body.as_deref(), Some("second thought"));
        assert!(reply.turn.is_none());

        let threaded = discuss(
            Cli::try_parse_from([
                "heddle",
                "discuss",
                "--id",
                "disc-01a0afc6",
                "--turn",
                "2",
                "reply to that turn",
            ])
            .expect("discuss --id --turn"),
        );
        assert_eq!(threaded.turn, Some(2));
        assert_eq!(threaded.body.as_deref(), Some("reply to that turn"));

        let from_file = discuss(
            Cli::try_parse_from([
                "heddle",
                "discuss",
                "--new",
                "--path",
                "src/lib.rs",
                "--file",
                "why.md",
            ])
            .expect("discuss --file body"),
        );
        assert_eq!(
            from_file.file.as_deref(),
            Some(std::path::Path::new("why.md"))
        );
        assert!(from_file.body.is_none());
    }

    #[test]
    fn new_and_id_are_exclusive() {
        assert!(
            Cli::try_parse_from([
                "heddle",
                "discuss",
                "--new",
                "--id",
                "disc-01a0afc6",
                "body",
            ])
            .is_err(),
            "--new and --id must conflict"
        );
    }

    #[test]
    fn copy_open_argv_is_not_a_turn_parser() {
        assert!(
            Cli::try_parse_from(["heddle", "discuss", "turn", "src/auth.rs", "verify", "body",])
                .is_err(),
            "FILE SYMBOL BODY must not parse as discuss turn"
        );
        let turn = discuss(
            Cli::try_parse_from(["heddle", "discuss", "turn", "disc-id", "body"]).expect("id argv"),
        );
        match turn.command {
            Some(DiscussCommands::Turn(args)) => {
                assert_eq!(args.discussion_id, "disc-id");
                assert_eq!(args.body, "body");
            }
            _ => panic!("expected discuss turn"),
        }
    }

    #[test]
    fn show_and_resolve_take_an_id_not_file_symbol() {
        let shown = discuss(
            Cli::try_parse_from(["heddle", "discuss", "show", "disc-id"]).expect("show id"),
        );
        match shown.command {
            Some(DiscussCommands::Show(args)) => {
                assert_eq!(args.discussion_id, "disc-id");
            }
            _ => panic!("expected discuss show"),
        }
        assert!(
            Cli::try_parse_from(["heddle", "discuss", "show", "src/auth.rs", "verify"]).is_err(),
            "FILE SYMBOL must not parse as discuss show"
        );
        let resolved = discuss(
            Cli::try_parse_from([
                "heddle",
                "discuss",
                "resolve",
                "disc-id",
                "--dismiss",
                "--reason",
                "done",
            ])
            .expect("resolve id"),
        );
        match resolved.command {
            Some(DiscussCommands::Resolve(args)) => {
                assert_eq!(args.discussion_id, "disc-id");
            }
            _ => panic!("expected discuss resolve"),
        }
    }

    #[test]
    fn resolve_dismiss_and_by_edit_shorthands_set_mode() {
        let dismiss = discuss(
            Cli::try_parse_from([
                "heddle",
                "discuss",
                "resolve",
                "disc-id",
                "--dismiss",
                "--reason",
                "done",
            ])
            .expect("dismiss shorthand"),
        );
        match dismiss.command {
            Some(DiscussCommands::Resolve(args)) => {
                assert!(matches!(
                    args.resolved_mode(),
                    Some(ResolveModeArg::Dismiss)
                ));
                assert_eq!(args.reason.as_deref(), Some("done"));
            }
            _ => panic!("expected discuss resolve"),
        }
        let by_edit = discuss(
            Cli::try_parse_from(["heddle", "discuss", "resolve", "disc-id", "--by-edit"])
                .expect("by-edit shorthand"),
        );
        match by_edit.command {
            Some(DiscussCommands::Resolve(args)) => {
                assert!(matches!(args.resolved_mode(), Some(ResolveModeArg::ByEdit)));
            }
            _ => panic!("expected discuss resolve"),
        }
    }

    #[test]
    fn append_parses_as_hidden_alias() {
        let append = discuss(
            Cli::try_parse_from(["heddle", "discuss", "append", "disc-id", "body"])
                .expect("append still parses so the handler can hint --id"),
        );
        match append.command {
            Some(DiscussCommands::Append(args)) => {
                assert_eq!(args.discussion_id.as_deref(), Some("disc-id"));
                assert_eq!(args.body.as_deref(), Some("body"));
            }
            _ => panic!("expected discuss append"),
        }
    }

    #[test]
    fn hidden_open_still_accepts_legacy_positionals() {
        let opened = discuss(
            Cli::try_parse_from([
                "heddle",
                "discuss",
                "open",
                "src/lib.rs",
                "greet",
                "why greet?",
            ])
            .expect("hidden open positionals"),
        );
        match opened.command {
            Some(DiscussCommands::Open(args)) => {
                assert_eq!(args.positional_file.as_deref(), Some("src/lib.rs"));
                assert_eq!(args.positional_symbol.as_deref(), Some("greet"));
                assert_eq!(args.body.as_deref(), Some("why greet?"));
            }
            _ => panic!("expected discuss open"),
        }
    }

    #[test]
    fn list_filters_by_path_not_file_body() {
        let listed = discuss(
            Cli::try_parse_from(["heddle", "discuss", "list", "--path", "src/lib.rs"])
                .expect("list --path"),
        );
        match listed.command {
            Some(DiscussCommands::List(args)) => {
                assert_eq!(args.path.as_deref(), Some("src/lib.rs"));
            }
            _ => panic!("expected discuss list"),
        }
        let aliased = discuss(
            Cli::try_parse_from(["heddle", "discuss", "list", "--file", "src/lib.rs"])
                .expect("list --file alias"),
        );
        match aliased.command {
            Some(DiscussCommands::List(args)) => {
                assert_eq!(args.path.as_deref(), Some("src/lib.rs"));
            }
            _ => panic!("expected discuss list"),
        }
    }
}
