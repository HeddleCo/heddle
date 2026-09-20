// SPDX-License-Identifier: Apache-2.0
//! `heddle discuss` — durable repository collaboration.

use clap::{Args, Subcommand};

use super::{AuthoredMessageArgs, CodeScopeArgs, HistoricalRevisionArgs, RemoteChoiceArgs};

/// `heddle discuss` — every action is a subcommand.
#[derive(Clone, Debug, Args)]
pub struct DiscussArgs {
    #[command(subcommand)]
    pub command: DiscussCommands,
}

#[derive(Clone, Debug, Subcommand)]
pub enum DiscussCommands {
    /// Open a new discussion.
    New(DiscussNewArgs),
    /// Reply to an existing discussion.
    Reply(DiscussReplyArgs),
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
pub struct DiscussNewArgs {
    #[command(flatten)]
    pub scope: CodeScopeArgs,
    #[command(flatten)]
    pub revision: HistoricalRevisionArgs,
    #[command(flatten)]
    pub message: AuthoredMessageArgs,
    /// Human-readable summary. Defaults to the first line of the first turn.
    #[arg(long)]
    pub title: Option<String>,
    /// Visibility: `public` | `internal` | `team:NAME` | `restricted:LABEL` | `private:LABEL`.
    #[arg(long)]
    pub visibility: Option<String>,
    /// Attach the discussion to a thread ref while keeping its code anchor.
    #[arg(long, value_name = "REF")]
    pub thread: Option<String>,
}

#[derive(Clone, Debug, Args)]
pub struct DiscussReplyArgs {
    /// Discussion id (short or full `disc-` id).
    #[arg(value_name = "ID")]
    pub discussion_id: String,
    #[command(flatten)]
    pub message: AuthoredMessageArgs,
    /// Parent turn number (1-indexed). Defaults to the latest head.
    #[arg(long)]
    pub turn: Option<u32>,
}

#[derive(Clone, Debug, Args)]
pub struct DiscussResolveArgs {
    /// Discussion id (short or full `disc-` id).
    #[arg(value_name = "ID")]
    pub discussion_id: String,
    /// Resolution kind: `by-edit`, `dismiss`, or `into-annotation`.
    #[arg(long, value_enum)]
    pub mode: ResolveModeArg,
    /// For `by-edit`: state containing the edit (defaults to HEAD).
    #[command(flatten)]
    pub revision: HistoricalRevisionArgs,
    /// For `dismiss`: non-empty reason.
    #[arg(long)]
    pub reason: Option<String>,
    /// For `into-annotation`: annotation content (`--body` or `--file`).
    #[command(flatten)]
    pub message: AuthoredMessageArgs,
    /// For `into-annotation`: constraint, invariant, or rationale (defaults to rationale).
    #[arg(long, value_parser = ["constraint", "invariant", "rationale"])]
    pub kind: Option<String>,
    /// For `into-annotation`: annotation tag (can be repeated).
    #[arg(long)]
    pub tag: Vec<String>,
}

#[derive(Clone, Debug, clap::ValueEnum)]
pub enum ResolveModeArg {
    ByEdit,
    Dismiss,
    IntoAnnotation,
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
    #[command(flatten)]
    pub scope: CodeScopeArgs,
    /// Filter by the state named in the discussion anchor.
    #[command(flatten)]
    pub revision: HistoricalRevisionArgs,
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
    #[command(flatten)]
    pub remote_choice: RemoteChoiceArgs,
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
    fn writes_are_subcommands() {
        let opened = discuss(
            Cli::try_parse_from([
                "heddle",
                "discuss",
                "new",
                "--path",
                "src/lib.rs",
                "--symbol",
                "greet",
                "--body",
                "why greet?",
            ])
            .expect("discuss new"),
        );
        match opened.command {
            DiscussCommands::New(args) => {
                assert_eq!(args.scope.path.as_deref(), Some("src/lib.rs"));
                assert_eq!(args.scope.symbol.as_deref(), Some("greet"));
                assert_eq!(args.message.body.as_deref(), Some("why greet?"));
            }
            _ => panic!("expected discuss new"),
        }

        let reply = discuss(
            Cli::try_parse_from([
                "heddle",
                "discuss",
                "reply",
                "disc-01a0afc6",
                "--body",
                "second thought",
            ])
            .expect("discuss reply"),
        );
        match reply.command {
            DiscussCommands::Reply(args) => {
                assert_eq!(args.discussion_id, "disc-01a0afc6");
                assert_eq!(args.message.body.as_deref(), Some("second thought"));
                assert!(args.turn.is_none());
            }
            _ => panic!("expected discuss reply"),
        }

        let threaded = discuss(
            Cli::try_parse_from([
                "heddle",
                "discuss",
                "reply",
                "disc-01a0afc6",
                "--turn",
                "2",
                "--body",
                "reply to that turn",
            ])
            .expect("discuss reply --turn"),
        );
        match threaded.command {
            DiscussCommands::Reply(args) => {
                assert_eq!(args.turn, Some(2));
                assert_eq!(args.message.body.as_deref(), Some("reply to that turn"));
            }
            _ => panic!("expected discuss reply"),
        }

        let from_file = discuss(
            Cli::try_parse_from([
                "heddle",
                "discuss",
                "new",
                "--path",
                "src/lib.rs",
                "--file",
                "why.md",
            ])
            .expect("discuss new --file"),
        );
        match from_file.command {
            DiscussCommands::New(args) => {
                assert_eq!(
                    args.message.file.as_deref(),
                    Some(std::path::Path::new("why.md"))
                );
                assert!(args.message.body.is_none());
            }
            _ => panic!("expected discuss new"),
        }
    }

    #[test]
    fn write_modes_on_parent_are_gone() {
        for argv in [
            [
                "heddle",
                "discuss",
                "--new",
                "--path",
                "src/lib.rs",
                "--body",
                "x",
            ]
            .as_slice(),
            ["heddle", "discuss", "--id", "disc-01a0afc6", "--body", "x"].as_slice(),
            [
                "heddle",
                "discuss",
                "open",
                "src/lib.rs",
                "greet",
                "why greet?",
            ]
            .as_slice(),
        ] {
            assert!(
                Cli::try_parse_from(argv).is_err(),
                "{argv:?} must not parse"
            );
        }
    }

    #[test]
    fn resolve_uses_mode_only() {
        let resolved = discuss(
            Cli::try_parse_from([
                "heddle", "discuss", "resolve", "disc-id", "--mode", "dismiss", "--reason", "done",
            ])
            .expect("resolve --mode dismiss"),
        );
        match resolved.command {
            DiscussCommands::Resolve(args) => {
                assert!(matches!(args.mode, ResolveModeArg::Dismiss));
                assert_eq!(args.reason.as_deref(), Some("done"));
            }
            _ => panic!("expected discuss resolve"),
        }
        assert!(
            Cli::try_parse_from([
                "heddle",
                "discuss",
                "resolve",
                "disc-id",
                "--dismiss",
                "--reason",
                "done",
            ])
            .is_err(),
            "--dismiss synonym is removed"
        );
        assert!(
            Cli::try_parse_from(["heddle", "discuss", "resolve", "disc-id", "--by-edit"]).is_err(),
            "--by-edit synonym is removed"
        );
        let into = discuss(
            Cli::try_parse_from([
                "heddle",
                "discuss",
                "resolve",
                "disc-id",
                "--mode",
                "into-annotation",
                "--body",
                "keep this",
            ])
            .expect("resolve into-annotation"),
        );
        match into.command {
            DiscussCommands::Resolve(args) => {
                assert!(matches!(args.mode, ResolveModeArg::IntoAnnotation));
                assert_eq!(args.message.body.as_deref(), Some("keep this"));
            }
            _ => panic!("expected discuss resolve"),
        }
    }

    #[test]
    fn list_filters_by_path() {
        let listed = discuss(
            Cli::try_parse_from(["heddle", "discuss", "list", "--path", "src/lib.rs"])
                .expect("list --path"),
        );
        match listed.command {
            DiscussCommands::List(args) => {
                assert_eq!(args.scope.path.as_deref(), Some("src/lib.rs"));
            }
            _ => panic!("expected discuss list"),
        }
        assert!(
            Cli::try_parse_from(["heddle", "discuss", "list", "--file", "src/lib.rs"]).is_err(),
            "list --file is not a path filter"
        );
    }
}
