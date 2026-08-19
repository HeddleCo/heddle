// SPDX-License-Identifier: Apache-2.0
//! Semantic-analysis command surface.

use clap::{Subcommand, ValueEnum};

#[derive(Subcommand, Clone)]
pub enum SemanticCommands {
    /// Compare the symbols stored in two states' attached semantic indexes.
    Diff {
        /// Earlier state or revision.
        a: String,
        /// Later state or revision.
        b: String,
    },
    /// Aggregate semantic-change events across recent history and
    /// surface the files or functions with the most activity.
    ///
    /// Use this to find: review-worthy hot spots ("which files are
    /// churning"), API stability signals (`--kind signature_changed`
    /// for a list of "your unstable surface area"), and annotation
    /// candidates (`--by function --include-actors` for "functions
    /// many people touched recently — context would help").
    Hot {
        /// State to start the walk from. Defaults to HEAD.
        #[arg(long)]
        from: Option<String>,
        /// Walk at most this many state pairs. Higher = more signal,
        /// linearly more compute. Default tuned for sub-10s runs on
        /// typical project history.
        #[arg(long, default_value_t = 200)]
        limit: usize,
        /// What to bucket on.
        #[arg(long, value_enum, default_value_t = HotSpotKeyArg::File)]
        by: HotSpotKeyArg,
        /// Restrict to specific event kinds. Repeat the flag for
        /// multiple kinds. No flag = all kinds.
        #[arg(long = "kind")]
        kinds: Vec<HotEventKindArg>,
        /// Substring path filter — include only events whose path
        /// contains any of these substrings. Repeatable.
        #[arg(long = "include")]
        include_paths: Vec<String>,
        /// Substring path filter — exclude events whose path contains
        /// any of these substrings. Repeatable.
        #[arg(long = "exclude")]
        exclude_paths: Vec<String>,
        /// Number of slots to print. Default 20.
        #[arg(long, default_value_t = 20)]
        top: usize,
        /// Include the per-actor histogram for each hot-spot. Slower
        /// (string allocation per event) but turns the output into
        /// "and here's who's been touching it."
        #[arg(long)]
        include_actors: bool,
    },
    /// Query persisted refs-of, callers-of, or importers-of at a state.
    ///
    /// Walks the attached semantic index and importer index only — never
    /// parses source. Time-travel is free: pass `--at` for any indexed state.
    Refs {
        /// State to query. Defaults to HEAD.
        #[arg(long = "at")]
        at: Option<String>,
        /// Report files that import this path (`importers_of`).
        #[arg(long, conflicts_with_all = ["callers", "anchor"])]
        importers: Option<String>,
        /// Restrict refs-of to call edges (`callers_of`).
        #[arg(long)]
        callers: bool,
        /// Symbol anchor as `path:symbol` (for example `src/api.rs:greet`).
        #[arg(required_unless_present = "importers")]
        anchor: Option<String>,
    },
    /// Backfill the content-addressed merkle semantic index over history.
    ///
    /// Native captures already index eagerly; this backfills states that
    /// predate the index (notably git-lane imports). Runs oldest-first and
    /// is restartable — rerun after an interruption and it resumes.
    Index {
        /// Recompute every state's index, superseding any existing one
        /// (e.g. after a grammar or extractor upgrade). Without this, only
        /// states missing an index are computed.
        #[arg(long)]
        all: bool,
    },
}

/// What dimension to group events on. Mirrors
/// [`semantic::HotSpotKey`] one-to-one.
#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum HotSpotKeyArg {
    File,
    Function,
}

/// Coarse classification mirroring [`semantic::HotEventKind`].
/// `clap`'s `ValueEnum` provides the kebab-case CLI spelling
/// (`file-modified`, `function-extracted`, etc.) automatically.
#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum HotEventKindArg {
    FileAdded,
    FileDeleted,
    FileModified,
    FileRenamed,
    FunctionExtracted,
    FunctionDeleted,
    FunctionRenamed,
    FunctionModified,
    FunctionMoved,
    SignatureChanged,
    DependencyChanged,
}
