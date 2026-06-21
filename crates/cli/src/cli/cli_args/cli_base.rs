// SPDX-License-Identifier: Apache-2.0
//! Base CLI flags.
//!
//! ## Short-flag conventions
//!
//! The CLI is small and the short forms have to mean the same thing
//! everywhere they appear. The table below is the source of truth — new
//! verbs should reuse these letters before claiming new ones. Verbs that
//! diverge (e.g. `-n` for `--steps` on `undo`/`redo` vs `--limit` on
//! `log`/`list`) keep the muscle memory consistent within the verb's own
//! family: "n" is always "how many," "m" is always "message."
//!
//! | Short | Long(s)                           | Used by                       |
//! |-------|-----------------------------------|-------------------------------|
//! | `-m`  | `--message`, `--intent`           | capture, merge, revert,       |
//! |       |                                   | cherry-pick, checkpoint,      |
//! |       |                                   | stash push, context set/edit, |
//! |       |                                   | discuss append                |
//! | `-n`  | `--limit` (queries),              | log, list, query (limit);     |
//! |       | `--steps` (undo/redo)             | undo, redo (steps)            |
//! | `-f`  | `--force`                         | capture, push, goto, clean,   |
//! |       |                                   | cherry-pick, rebase           |
//! | `-s`  | `--short`                         | status                        |
//! | `-U`  | `--unified`                       | diff                          |
//! | `-C`  | `--repo`                          | global                        |
//! | `-v`  | `--verbose` (repeatable)          | global                        |
//! | `-q`  | `--quiet`                         | global                        |
//!
//! Renames are out of scope — scripts written against the surface MUST
//! keep working. Add a new short alias only when the letter is already
//! reserved for that semantic in the table above.

use clap::Parser;
use cli_shared::OutputMode;

use super::Commands;

/// Heddle: An AI-native version control system.
#[derive(Parser)]
#[command(name = "heddle")]
#[command(author, version, about, long_about = None)]
// We ship our own `Help` subcommand (curated everyday/advanced
// surface + topic pages). clap's auto-generated `help` subcommand
// would shadow it; turn it off so `heddle help [topic]` reaches our
// printer instead.
#[command(disable_help_subcommand = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    // This is a `global = true` arg, so clap stamps its help onto every
    // subcommand's --help. Keep it to ONE line; the full contract
    // (json vs json-compact fields, no-TTY-autodetect guarantee) lives in
    // `heddle help output-formats` (help.rs OUTPUT_FORMATS_TOPIC) and the
    // top-level `heddle help` Output paragraph, stated exactly once each
    // (heddle#652).
    /// Output format: `text` (default), `json`, or `json-compact`. See `heddle help output-formats`
    #[arg(long, global = true, value_enum)]
    pub output: Option<OutputMode>,

    /// Disable colored output.
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Repository path (default: find .heddle in ancestors).
    #[arg(short = 'C', long, global = true, value_name = "PATH")]
    pub repo: Option<std::path::PathBuf>,

    /// Increase verbosity.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Decrease verbosity.
    #[arg(short, long, global = true)]
    pub quiet: bool,

    // Global like --output, and revealed on every `supports_op_id`
    // command's help — keep it to one line (heddle#652). Replay semantics
    // (`supports_op_id` advertisement, same-body replay, typed conflicts)
    // live in `heddle help operation-ids`. Hidden from default `--help` to
    // keep the human surface uncluttered.
    /// Operation id (UUID v4) for idempotent retries. See `heddle help operation-ids`
    #[arg(long, global = true, env = "HEDDLE_OPERATION_ID", hide = true)]
    pub op_id: Option<String>,
}

impl Cli {
    /// Open the Heddle repository the command should act on: the `--repo`
    /// path if given, otherwise the current working directory (resolved
    /// lazily so a supplied `--repo` never touches the cwd).
    pub fn open_repo(&self) -> anyhow::Result<repo::Repository> {
        use anyhow::Context as _;
        let cwd;
        let repo_path = match self.repo.as_ref() {
            Some(path) => path,
            None => {
                cwd = std::env::current_dir().context("get current working directory")?;
                &cwd
            }
        };
        repo::Repository::open(repo_path).context("open Heddle repository")
    }
}
