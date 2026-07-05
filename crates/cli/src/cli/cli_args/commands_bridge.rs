// SPDX-License-Identifier: Apache-2.0
//! Bridge command definitions.

use std::path::PathBuf;

use clap::Subcommand;

/// Source for a git import: either a local filesystem path or a URL that
/// sley can fetch from.
///
/// We discriminate by inspecting the input string: anything containing
/// `://` (https/ssh/git/file URLs) or starting with `git@` (ssh shorthand)
/// is treated as a URL; everything else is a local path. This keeps the
/// rules predictable — `/tmp/foo` always means a path, and a stray
/// `git@host:path` shorthand never gets misread as a relative path.
#[derive(Debug, Clone)]
pub enum GitSource {
    Path(PathBuf),
    Url(String),
}

impl GitSource {
    pub fn parse(s: &str) -> Result<Self, String> {
        if s.contains("://") || s.starts_with("git@") {
            Ok(GitSource::Url(s.to_string()))
        } else {
            Ok(GitSource::Path(PathBuf::from(s)))
        }
    }

    pub fn display(&self) -> String {
        match self {
            GitSource::Path(p) => p.display().to_string(),
            GitSource::Url(u) => u.clone(),
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
    /// scripts; other `--output json` outputs intentionally omit it.
    Status,
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
    /// Walks local branches and tags by default. To import remote-tracking
    /// refs (`refs/remotes/*`), name them explicitly with `--ref`.
    /// Reflog-only history is not part of the public bridge import surface.
    ///
    /// `--path` accepts either a local filesystem path
    /// (`/tmp/some-repo`, `./.git`) or a git URL — `https://...`,
    /// `ssh://...`, `git://...`, `git@host:owner/repo.git`, or
    /// `file://...`. URL imports are cloned into a heddle-managed temp
    /// directory and then imported the same way local imports are.
    /// Authentication for private URL sources uses Git-compatible
    /// credential files/config and the host SSH agent when available.
    Import {
        /// Local path or git URL to import from.
        #[arg(short, long, value_parser = parse_git_source)]
        path: Option<GitSource>,

        /// Ref names to import (repeatable). Scopes the import to the
        /// listed branches, tags, or remote-tracking refs; omit to
        /// import all branches and tags.
        #[arg(long = "ref", value_name = "REF")]
        refs: Vec<String>,

        /// Accept git tree entries Heddle cannot represent losslessly.
        ///
        /// By default import fails on the first unrepresentable tree entry
        /// and names the offending path. With this flag, import restores the
        /// historical drop/convert behavior and prints an end-of-run summary
        /// of every affected entry.
        #[arg(long)]
        lossy: bool,
    },

    /// Bidirectional sync with Git (export + import).
    Sync {
        /// Local path or git URL to sync with.
        #[arg(short, long, value_parser = parse_git_source)]
        path: Option<GitSource>,
    },

    /// Preview a recovery path when a Git branch and Heddle thread diverge.
    Reconcile {
        /// Which local side should be treated as authoritative when applying.
        ///
        /// Omit with `--preview` to inspect both local repair choices without
        /// changing refs, remotes, index, or worktree files.
        #[arg(long, value_parser = ["git", "heddle"])]
        prefer: Option<String>,
        /// Branch/ref to reconcile.
        #[arg(long = "ref", value_name = "BRANCH")]
        ref_name: String,
        /// Show the planned recovery without changing refs.
        #[arg(long)]
        preview: bool,
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

    /// Mine local AI-coding-agent sessions (Claude / Codex / OpenCode)
    /// for reasoning notecards and attach them as `context` annotations
    /// to the matching imported states. Requires `bridge git import` to
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
