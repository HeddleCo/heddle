// SPDX-License-Identifier: Apache-2.0
//! Bridge command definitions.

use std::path::PathBuf;

use clap::Subcommand;
use gix::bstr::ByteSlice;

/// Source for a git import: either a local filesystem path or a URL that
/// gix can fetch from.
///
/// We discriminate by inspecting the input string: anything containing
/// `://` (https/ssh/git/file URLs) or starting with `git@` (ssh shorthand)
/// is treated as a URL; everything else is a local path. This keeps the
/// rules predictable — `/tmp/foo` always means a path, and a stray
/// `git@host:path` shorthand never gets misread as a relative path.
#[derive(Debug, Clone)]
pub enum GitSource {
    Path(PathBuf),
    Url(gix::Url),
}

impl GitSource {
    pub fn parse(s: &str) -> Result<Self, String> {
        if s.contains("://") || s.starts_with("git@") {
            let url = gix::url::parse(s.as_bytes().as_bstr()).map_err(|e| e.to_string())?;
            Ok(GitSource::Url(url))
        } else {
            Ok(GitSource::Path(PathBuf::from(s)))
        }
    }

    pub fn display(&self) -> String {
        match self {
            GitSource::Path(p) => p.display().to_string(),
            GitSource::Url(u) => u.to_string(),
        }
    }
}

fn parse_git_source(s: &str) -> Result<GitSource, String> {
    GitSource::parse(s)
}

#[derive(Subcommand, Clone)]
pub enum BridgeCommands {
    /// Git bridge operations.
    Git {
        #[command(subcommand)]
        command: GitCommands,
    },
}

#[derive(Subcommand, Clone)]
pub enum GitCommands {
    /// Show the current state of the Git overlay bridge.
    ///
    /// Reports the import-hint surface (which Git branches are visible
    /// only on the Git side and need a `bridge import` run), the active
    /// branch on the Git side, and any pending bridge operation. This
    /// is the canonical place to consume bridge-status information for
    /// scripts; other `--json` outputs intentionally omit it.
    Status,
    /// Initialize Git mirror.
    Init {
        /// Path to existing Git repository (optional).
        #[arg(long)]
        path: Option<std::path::PathBuf>,
    },

    /// Export Heddle states to Git.
    ///
    /// Writes a complete bare git repository at `--destination` containing
    /// every reachable Heddle state as a git commit, with branches and tags
    /// mirroring Heddle's threads and markers.
    Export {
        /// Destination path for the exported git repository. Must be writable;
        /// will be initialized as a bare repo if it does not already exist.
        #[arg(short, long)]
        destination: Option<std::path::PathBuf>,
    },

    /// Import Git commits to Heddle.
    ///
    /// Walks **local branches and tags only** in the source repository.
    /// Remote-tracking refs (`refs/remotes/*`) and reflog history are
    /// ignored; for those, use `bridge git ingest` instead.
    ///
    /// `--path` accepts either a local filesystem path
    /// (`/tmp/some-repo`, `./.git`) or a git URL — `https://...`,
    /// `ssh://...`, `git://...`, `git@host:owner/repo.git`, or
    /// `file://...`. URL imports are cloned into a heddle-managed temp
    /// directory and then imported the same way local imports are.
    /// Authentication for private URL sources uses the standard git
    /// credential helpers (~/.git-credentials, ssh-agent, etc.).
    Import {
        /// Local path or git URL to import from.
        #[arg(short, long, value_parser = parse_git_source)]
        path: Option<GitSource>,

        /// Optional ref names to import (default: all branches/tags).
        /// Codex's git-overlay foundation added this flag to scope
        /// imports to a specific branch or remote-tracking ref —
        /// kept here on the rebase onto main.
        #[arg(long = "ref", value_name = "REF")]
        refs: Vec<String>,
    },

    /// Bidirectional sync with Git (export + import).
    Sync {
        /// Local path or git URL to sync with.
        #[arg(short, long, value_parser = parse_git_source)]
        path: Option<GitSource>,
    },

    /// Push to Git remote.
    Push {
        /// Remote name (default: origin).
        remote: Option<String>,
    },

    /// Pull from Git remote.
    Pull {
        /// Remote name (default: origin).
        remote: Option<String>,
    },

    /// Deep import: walk every git ref (local + tags + remotes) and the
    /// reflog, translate each commit into a Heddle state with full agent
    /// attribution, and replay reflog entries into the oplog. Distinct
    /// from `bridge git import` which only mirrors local branches.
    /// Requires the `ingest` feature (on by default).
    #[cfg(feature = "ingest")]
    Ingest {
        /// Source git repository (local path or URL) to import from.
        #[arg(long, value_parser = parse_git_source)]
        path: GitSource,
    },

    /// Mine local AI-coding-agent sessions (Claude / Codex / OpenCode)
    /// for reasoning notecards and attach them as `context` annotations
    /// to the matching imported states. Requires `bridge git ingest` to
    /// have already run (needs the SHA map sidecar).
    /// Requires the `ingest` feature (on by default).
    #[cfg(feature = "ingest")]
    Reason {
        /// Source git repository the transcripts are about.
        #[arg(long)]
        path: std::path::PathBuf,
        /// Cap candidates per commit. Higher = more coverage at the cost
        /// of cross-attribution. Default tuned for typical dogfood runs.
        #[arg(long, default_value_t = 5)]
        max_sessions_per_commit: usize,
        /// Drop sessions below this confidence (file-overlap × 0.65 +
        /// time-fit × 0.25 + provider-hint × 0.10). Below ~0.20 the
        /// matcher trips false positives; above ~0.50 it filters out
        /// borderline-but-correct matches.
        #[arg(long, default_value_t = 0.20)]
        min_match_confidence: f32,
        /// Limit how many commits the reason pass walks. Useful while
        /// tuning extraction knobs against a small recent window.
        #[arg(long)]
        limit: Option<usize>,
        /// Override the Claude transcript store. Empty string disables.
        #[arg(long = "claude-home")]
        claude_home: Option<String>,
        /// Override the Codex transcript store. Empty string disables.
        #[arg(long = "codex-home")]
        codex_home: Option<String>,
        /// Override the OpenCode data dir. Empty string disables.
        #[arg(long = "opencode-home")]
        opencode_home: Option<String>,
        /// Don't write annotations — just report what would happen.
        #[arg(long)]
        dry_run: bool,
    },
}