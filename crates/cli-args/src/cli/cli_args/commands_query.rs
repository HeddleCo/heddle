// SPDX-License-Identifier: Apache-2.0
//! `heddle query` — structured query over the operation log.

use clap::Args;

use super::HistoricalRevisionArgs;

/// Line-by-line attribution for a tracked file (`heddle blame <path>`).
#[derive(Clone, Debug, Args)]
pub struct BlameArgs {
    /// Tracked file to attribute.
    #[arg(value_name = "PATH")]
    pub path: String,
    #[command(flatten)]
    pub revision: HistoricalRevisionArgs,
    /// Include applicable context annotations.
    #[arg(long)]
    pub context: bool,
}

#[derive(Clone, Debug, Args)]
pub struct QueryArgs {
    /// Show line-by-line attribution for a tracked file.
    #[arg(long, value_name = "FILE")]
    pub attribution: Option<String>,
    /// State to inspect with `--attribution`. Accepts short or full
    /// state IDs, marker names, `HEAD`, `@`, or `HEAD~N`.
    #[arg(long, requires = "attribution")]
    pub state: Option<String>,
    /// Include applicable context annotations with `--attribution`.
    #[arg(long, requires = "attribution")]
    pub context: bool,
    /// Filter by actor email.
    #[arg(long)]
    pub actor: Option<String>,
    /// Lower bound. Accepts RFC3339 (`2026-05-04T12:00:00Z`) or
    /// humantime (`1h`, `2d`, `30m`).
    #[arg(long)]
    pub since: Option<String>,
    /// Upper bound, same formats as `--since`.
    #[arg(long)]
    pub until: Option<String>,
    /// Filter by signal kind (e.g. `novelty`, `invariant_adjacency`).
    #[arg(long)]
    pub signal: Option<String>,
    /// Filter by symbol (free-form `<file>:<symbol>` string).
    #[arg(long)]
    pub symbol: Option<String>,
    /// Filter by thread name.
    #[arg(long)]
    pub thread: Option<String>,
    /// Restrict to specific oplog verbs. Repeat to allow multiple.
    /// `capture` matches the stored `snapshot` verb; matching is case-insensitive.
    #[arg(long = "verb")]
    pub verbs: Vec<String>,
    /// Maximum hits to return.
    #[arg(long, default_value = "100")]
    pub limit: u32,
    /// Include checkpoint entries (excluded by default).
    #[arg(long)]
    pub include_checkpoints: bool,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::cli::{Cli, Commands};

    #[test]
    fn blame_parses_path_state_and_context() {
        match Cli::try_parse_from(["heddle", "blame", "src/auth.rs"])
            .expect("blame path")
            .command
        {
            Commands::Blame(args) => {
                assert_eq!(args.path, "src/auth.rs");
                assert!(args.revision.state.is_none());
                assert!(!args.context);
            }
            _ => panic!("expected blame"),
        }
        match Cli::try_parse_from([
            "heddle",
            "blame",
            "src/auth.rs",
            "--state",
            "HEAD",
            "--context",
        ])
        .expect("blame flags")
        .command
        {
            Commands::Blame(args) => {
                assert_eq!(args.revision.state.as_deref(), Some("HEAD"));
                assert!(args.context);
            }
            _ => panic!("expected blame"),
        }
    }
}
