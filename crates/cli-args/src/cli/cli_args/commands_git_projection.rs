// SPDX-License-Identifier: Apache-2.0
//! Git projection command definitions.

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

pub(crate) fn parse_git_source(s: &str) -> Result<GitSource, String> {
    GitSource::parse(s)
}

#[derive(Subcommand, Clone)]
pub enum ImportCommands {
    /// Import Git commits to Heddle.
    ///
    /// Walks local branches and tags by default. To import remote-tracking
    /// refs (`refs/remotes/*`), name them explicitly with `--ref`.
    Git {
        /// Local path or git URL to import from.
        #[arg(short, long, value_parser = parse_git_source)]
        path: Option<GitSource>,

        /// Ref names to import (repeatable). Scopes the import to the
        /// listed branches, tags, or remote-tracking refs; omit to
        /// import all branches and tags.
        #[arg(long = "ref", value_name = "REF")]
        refs: Vec<String>,

        /// Accept git tree entries Heddle cannot represent losslessly.
        #[arg(long)]
        lossy: bool,
    },
}

#[derive(Subcommand, Clone)]
pub enum ExportCommands {
    /// Export Heddle states to Git.
    ///
    /// Writes a complete bare Git repository at `--destination` containing
    /// every reachable Heddle state as a Git commit, with branches and tags
    /// mirroring Heddle's threads and markers.
    Git {
        /// Destination path for the exported Git repository. Must be writable;
        /// will be initialized as a bare repo if it does not already exist.
        #[arg(short, long)]
        destination: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand, Clone, Debug)]
pub enum SyncCommands {
    /// Bidirectional sync with Git (export + import).
    Git {
        /// Local path or git URL to sync with.
        #[arg(short, long, value_parser = parse_git_source)]
        path: Option<GitSource>,
    },
}
