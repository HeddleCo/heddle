// SPDX-License-Identifier: Apache-2.0
//! Shared clap Args types — one flag name, one meaning.
//!
//! Clap definitions, human help, and the machine catalog all flatten these
//! types. Behavior-contract tests below lock the shared meaning so the
//! surfaces cannot drift.

use std::path::{Path, PathBuf};

use clap::{Args, ValueEnum};

/// `--path` / `--symbol` / `--line` code scope.
///
/// `--path` names a file. `--symbol` and `--line` further pin that file;
/// either or both may be set, and both require `--path`.
#[derive(Clone, Debug, Default, Args)]
pub struct CodeScopeArgs {
    /// File path to act on.
    #[arg(long)]
    pub path: Option<String>,

    /// Anchor symbol. Requires `--path`.
    #[arg(long, requires = "path")]
    pub symbol: Option<String>,

    /// Anchor line (1-indexed). Requires `--path`. May combine with `--symbol`.
    #[arg(long, requires = "path")]
    pub line: Option<u32>,
}

impl CodeScopeArgs {
    /// True when any scope flag is set.
    pub fn is_set(&self) -> bool {
        self.path.is_some() || self.symbol.is_some() || self.line.is_some()
    }
}

/// Historical revision selector. Always `--state`.
///
/// Accepts short or full state IDs, marker names, `HEAD`, `@`, or `HEAD~N`.
/// Combined with `--path`, this is the revision to read; without `--path`,
/// it is the state-level target.
#[derive(Clone, Debug, Default, Args)]
pub struct HistoricalRevisionArgs {
    /// Historical revision or state-level target.
    #[arg(long)]
    pub state: Option<String>,
}

impl HistoricalRevisionArgs {
    pub fn as_deref(&self) -> Option<&str> {
        self.state.as_deref()
    }
}

/// Split `--path` / `--state` into a file target, a state target, and a
/// historical revision to read.
///
/// `--path` always names a file. `--state` with `--path` is the historical
/// revision of that file. `--state` alone is a state-level target.
pub fn split_path_and_revision<'a>(
    path: Option<&'a str>,
    state: Option<&'a str>,
) -> (Option<&'a str>, Option<&'a str>, Option<&'a str>) {
    match (path, state) {
        (Some(path), state) => (Some(path), None, state),
        (None, Some(state)) => (None, Some(state), None),
        (None, None) => (None, None, None),
    }
}

/// `--remote` flag with no injected default.
///
/// Resolution order (see [`RemoteChoiceArgs::requested`]): explicit flag,
/// else the configured default from `heddle remote set-default`, else an
/// actionable error. Never injects `origin`.
#[derive(Clone, Debug, Default, Args)]
pub struct RemoteChoiceArgs {
    /// Hosted remote name. Omit to use `heddle remote set-default`.
    #[arg(long)]
    pub remote: Option<String>,
}

impl RemoteChoiceArgs {
    /// Explicit `--remote` value, if the user passed one.
    pub fn requested(&self) -> Option<&str> {
        self.remote
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }
}

/// Inline `--body` / `-m` or `--file` authored text.
///
/// One body-input convention across discuss/context writes. `--body` and
/// `--file` conflict.
#[derive(Clone, Debug, Default, Args)]
pub struct AuthoredMessageArgs {
    /// Inline body text.
    #[arg(short = 'm', long = "body")]
    pub body: Option<String>,

    /// Read the body from a file.
    #[arg(long, value_name = "PATH", conflicts_with = "body")]
    pub file: Option<PathBuf>,
}

impl AuthoredMessageArgs {
    pub fn is_set(&self) -> bool {
        self.body.is_some() || self.file.is_some()
    }
}

/// Hosted server choice shared by grant/promote/import/claim-style commands.
#[derive(Clone, Debug, Default, Args)]
pub struct HostedServerArgs {
    /// Hosted Heddle server. Omit when the destination is a URL, or to use the configured default.
    #[arg(long)]
    pub server: Option<String>,
}

impl HostedServerArgs {
    pub fn as_deref(&self) -> Option<&str> {
        self.server.as_deref()
    }
}

/// `--dry-run`: perform no mutation.
#[derive(Clone, Debug, Default, Args)]
pub struct DryRunArgs {
    /// Perform no mutation.
    #[arg(long)]
    pub dry_run: bool,
}

impl DryRunArgs {
    pub fn enabled(&self) -> bool {
        self.dry_run
    }
}

/// Explicit clone protocol. Failures are not retried on the other protocol.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum CloneSourceArg {
    Git,
    Heddle,
}

impl CloneSourceArg {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Heddle => "heddle",
        }
    }
}

/// Derive a destination directory from a clone source when the user omitted one.
///
/// Returns `None` when the basename is missing or unsafe (`/`, `.`, `..`,
/// empty, or still contains a path separator after stripping a trailing
/// `.git`).
pub fn safe_clone_destination_basename(remote: &str) -> Option<String> {
    let path_part = if let Some(rest) = remote
        .strip_prefix("https://")
        .or_else(|| remote.strip_prefix("http://"))
        .or_else(|| remote.strip_prefix("ssh://"))
        .or_else(|| remote.strip_prefix("git://"))
        .or_else(|| remote.strip_prefix("file://"))
    {
        let rest = rest.strip_prefix("git@").unwrap_or(rest);
        match rest.split_once('/') {
            Some((_, path)) if !path.is_empty() => path,
            _ => return None,
        }
    } else {
        match remote.find(':') {
            Some(colon_pos) => {
                let prefix = &remote[..colon_pos];
                let rest = &remote[colon_pos + 1..];
                let is_windows_drive = prefix.len() == 1
                    && prefix
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_alphabetic())
                    && (rest.starts_with('\\') || rest.starts_with('/'));
                let prefix_has_separator = prefix.contains('/') || prefix.contains('\\');
                if is_windows_drive || prefix_has_separator {
                    remote
                } else if rest.is_empty() {
                    return None;
                } else {
                    rest
                }
            }
            None => remote,
        }
    };
    let is_sep = |c: char| c == '/' || c == '\\';
    let segment = path_part
        .trim_end_matches(is_sep)
        .rsplit(is_sep)
        .find(|part| !part.is_empty())
        .unwrap_or("");
    let name = segment.strip_suffix(".git").unwrap_or(segment);
    if name.is_empty()
        || name == "."
        || name == ".."
        || name == "~"
        || name == "/"
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || Path::new(name).is_absolute()
    {
        return None;
    }
    Some(name.to_string())
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::*;
    use crate::cli::{
        Cli, Commands, ContextCommands, DiscussCommands, ReviewCommands, ThreadCommands,
    };

    #[test]
    fn remote_choice_has_no_origin_default() {
        match Cli::try_parse_from(["heddle", "review", "approve", "feat"])
            .expect("approve without --remote")
            .command
        {
            Commands::Review {
                command: ReviewCommands::Approve(args),
            } => {
                assert!(args.remote_choice.requested().is_none());
            }
            _ => panic!("expected review approve"),
        }
        match Cli::try_parse_from([
            "heddle", "review", "approve", "feat", "--remote", "upstream",
        ])
        .expect("approve with --remote")
        .command
        {
            Commands::Review {
                command: ReviewCommands::Approve(args),
            } => {
                assert_eq!(args.remote_choice.requested(), Some("upstream"));
            }
            _ => panic!("expected review approve"),
        }
    }

    #[test]
    fn no_remote_flag_injects_origin() {
        let cmd = Cli::command();
        fn walk(command: &clap::Command) {
            for arg in command.get_arguments() {
                if arg.get_long() == Some("remote")
                    && (arg.get_short().is_some() || arg.get_long().is_some())
                    && !arg.is_positional()
                {
                    let defaults: Vec<_> = arg
                        .get_default_values()
                        .iter()
                        .map(|v| v.to_string_lossy().into_owned())
                        .collect();
                    assert!(
                        !defaults.iter().any(|v| v == "origin"),
                        "`{} --remote` must not default to origin (got {defaults:?})",
                        command.get_name()
                    );
                }
            }
            for sub in command.get_subcommands() {
                walk(sub);
            }
        }
        walk(&cmd);
    }

    #[test]
    fn scope_and_state_mean_the_same_on_discuss_context_and_blame() {
        match Cli::try_parse_from([
            "heddle",
            "discuss",
            "new",
            "--path",
            "src/auth.rs",
            "--symbol",
            "verify",
            "--line",
            "12",
            "--state",
            "HEAD~1",
            "--body",
            "why?",
        ])
        .expect("discuss new scope")
        .command
        {
            Commands::Discuss(args) => match args.command {
                DiscussCommands::New(new_args) => {
                    assert_eq!(new_args.scope.path.as_deref(), Some("src/auth.rs"));
                    assert_eq!(new_args.scope.symbol.as_deref(), Some("verify"));
                    assert_eq!(new_args.scope.line, Some(12));
                    assert_eq!(new_args.revision.state.as_deref(), Some("HEAD~1"));
                    assert_eq!(new_args.message.body.as_deref(), Some("why?"));
                }
                _ => panic!("expected discuss new"),
            },
            _ => panic!("expected discuss"),
        }

        match Cli::try_parse_from([
            "heddle",
            "context",
            "get",
            "--path",
            "src/auth.rs",
            "--symbol",
            "verify",
            "--line",
            "12",
            "--state",
            "HEAD~1",
        ])
        .expect("context get scope")
        .command
        {
            Commands::Context {
                command: ContextCommands::Get(args),
            } => {
                assert_eq!(args.scope.path.as_deref(), Some("src/auth.rs"));
                assert_eq!(args.scope.symbol.as_deref(), Some("verify"));
                assert_eq!(args.scope.line, Some(12));
                assert_eq!(args.revision.state.as_deref(), Some("HEAD~1"));
                let (path, state_target, historical) = split_path_and_revision(
                    args.scope.path.as_deref(),
                    args.revision.state.as_deref(),
                );
                assert_eq!(path, Some("src/auth.rs"));
                assert!(state_target.is_none());
                assert_eq!(historical, Some("HEAD~1"));
            }
            _ => panic!("expected context get"),
        }

        match Cli::try_parse_from(["heddle", "blame", "src/auth.rs", "--state", "HEAD~1"])
            .expect("blame --state")
            .command
        {
            Commands::Blame(args) => {
                assert_eq!(args.path, "src/auth.rs");
                assert_eq!(args.revision.state.as_deref(), Some("HEAD~1"));
            }
            _ => panic!("expected blame"),
        }

        assert!(
            Cli::try_parse_from([
                "heddle",
                "context",
                "get",
                "--path",
                "src/auth.rs",
                "--ref",
                "HEAD",
            ])
            .is_err(),
            "historical selector is --state, not --ref"
        );
    }

    #[test]
    fn dry_run_is_the_no_mutation_flag() {
        match Cli::try_parse_from(["heddle", "undo", "--dry-run"])
            .expect("undo --dry-run")
            .command
        {
            Commands::Undo(args) => assert!(args.dry_run.enabled()),
            _ => panic!("expected undo"),
        }
        match Cli::try_parse_from(["heddle", "thread", "absorb", "child", "--dry-run"])
            .expect("absorb --dry-run")
            .command
        {
            Commands::Thread {
                command: ThreadCommands::Absorb(args),
            } => assert!(args.dry_run.enabled()),
            _ => panic!("expected absorb"),
        }
        assert!(
            Cli::try_parse_from(["heddle", "undo", "--preview"]).is_err(),
            "--preview is not a dry-run alias"
        );
        assert!(
            Cli::try_parse_from(["heddle", "thread", "absorb", "child", "--preview"]).is_err(),
            "--preview is not a dry-run alias"
        );
    }

    #[test]
    fn safe_clone_basename_is_unambiguous_or_none() {
        assert_eq!(
            safe_clone_destination_basename("https://host/acme/widgets.git").as_deref(),
            Some("widgets")
        );
        assert_eq!(
            safe_clone_destination_basename("git@host:acme/widgets.git").as_deref(),
            Some("widgets")
        );
        assert!(safe_clone_destination_basename("https://host/").is_none());
        assert!(safe_clone_destination_basename(".").is_none());
        assert!(safe_clone_destination_basename("..").is_none());
    }
}
