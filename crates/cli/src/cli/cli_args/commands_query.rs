// SPDX-License-Identifier: Apache-2.0
//! `heddle query` — structured query over the operation log (A10).

use clap::Args;

#[derive(Clone, Debug, Args)]
pub struct QueryArgs {
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
    #[arg(long = "verb")]
    pub verbs: Vec<String>,
    /// Maximum hits to return.
    #[arg(long, default_value = "100")]
    pub limit: u32,
    /// Include checkpoint entries (excluded by default).
    #[arg(long)]
    pub include_checkpoints: bool,
}