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

use clap::{Parser, ValueEnum};

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

    /// Output format. `auto` (default) renders text on a TTY and JSON when piped;
    /// `json` and `text` override regardless of stream.
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

    /// Client-supplied operation id (UUID v4) for idempotent retries.
    ///
    /// Commands that advertise `supports_op_id: true` in
    /// `heddle commands --output json` return the original outcome on
    /// replay. Unset by default; agents supply one explicitly. Also
    /// reads `HEDDLE_OPERATION_ID` from the environment.
    ///
    /// Hidden from default `--help` to keep the human surface uncluttered;
    /// see `heddle help operation-ids` for the full agent-facing contract.
    #[arg(long, global = true, env = "HEDDLE_OPERATION_ID", hide = true)]
    pub op_id: Option<String>,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum OutputMode {
    Auto,
    Json,
    Text,
}
