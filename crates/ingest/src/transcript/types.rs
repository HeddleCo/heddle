// SPDX-License-Identifier: Apache-2.0
//! Core data types for the transcript matcher.
//!
//! A [`Transcript`] is a normalized view over either a Claude Code JSONL
//! session file or a Codex rollout file. The point of the normalization is
//! to give the matcher three signals it can rely on regardless of provider:
//!
//! 1. **Time window** — when the session was active.
//! 2. **Working directory** — where the agent was operating.
//! 3. **File touches** — which paths the agent edited, in roughly what
//!    order.
//!
//! Everything provider-specific (tool taxonomy, JSON shape, patch
//! grammar) is flattened before the matcher sees it.

use std::path::PathBuf;

use chrono::{DateTime, Utc};

/// Which agent produced the transcript. Used as a tiebreaker in the
/// matcher (a commit with `Co-Authored-By: Claude` prefers Claude
/// sessions) and is carried through into the `ReasoningPoint.evidence.provider`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Provider {
    Claude,
    Codex,
    OpenCode,
}

impl Provider {
    /// Lowercase wire name, matching [`crate::reasoning::ReasoningEvidence::provider`].
    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
            Provider::OpenCode => "opencode",
        }
    }
}

/// One file interaction inside a session. We capture reads as well as
/// writes because a commit's signal is "the agent was looking at this
/// code right before the commit landed" — reads are weaker evidence but
/// still worth counting when the overlap set is tiny.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileTouch {
    /// Absolute path as the agent saw it. The matcher normalizes against
    /// the repo root before comparing to commit paths.
    pub path: PathBuf,
    pub timestamp: DateTime<Utc>,
    pub kind: TouchKind,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TouchKind {
    /// File content was modified (Claude `Edit`/`Write`, Codex apply_patch
    /// Update/Add, or a shell redirect into the file).
    Write,
    /// File was read (Claude `Read`, Codex `cat`/`rg`-style commands).
    /// Weaker signal than `Write`.
    Read,
    /// File was deleted (Codex apply_patch `*** Delete File:`).
    Delete,
}

impl TouchKind {
    /// Weight applied when scoring overlap. Writes/deletes are stronger
    /// evidence than reads.
    pub fn weight(&self) -> f32 {
        match self {
            TouchKind::Write | TouchKind::Delete => 1.0,
            TouchKind::Read => 0.4,
        }
    }
}

/// A normalized agent session. Cheap to clone once constructed — the
/// heavy JSONL parse happens once in the loader.
#[derive(Clone, Debug)]
pub struct Transcript {
    pub provider: Provider,
    /// Stable id the agent framework assigned — UUID for both providers.
    pub session_id: String,
    /// The `.jsonl` file on disk, so the reasoning extractor can re-read
    /// specific turns without us having to carry the full message history
    /// in memory.
    pub source_path: PathBuf,
    /// Working directory the session ran from, when we could determine it.
    /// Unknown sessions are rare (corrupted meta) but legal — they just
    /// lose the cwd signal in the matcher.
    pub cwd: Option<PathBuf>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub turn_count: u32,
    /// In-order list of file interactions. Not deduplicated — the same
    /// file can appear many times across a session.
    pub files_touched: Vec<FileTouch>,
    /// For Codex sessions, the git commit the session started on.
    /// Unused for matching today but retained because it's a strong
    /// anchor for future "which session authored this state" queries.
    pub starting_commit: Option<String>,
}

impl Transcript {
    /// Deduplicated set of distinct paths touched, preserving first-seen
    /// order. Handy for the matcher's overlap computation, which doesn't
    /// care how many times a file was edited.
    pub fn distinct_paths(&self) -> Vec<&PathBuf> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for touch in &self.files_touched {
            if seen.insert(&touch.path) {
                out.push(&touch.path);
            }
        }
        out
    }

    /// `true` if `when` falls inside the session's active window, with a
    /// grace period on each side. Grace handles two common cases:
    /// (a) git's committer-time can precede the actual `git commit`
    ///     invocation by seconds when the commit happens near a turn
    ///     boundary, and
    /// (b) a session that crashes before writing the trailing event has
    ///     an `ended_at` slightly before the real finish.
    pub fn contains_time(&self, when: DateTime<Utc>, grace: chrono::Duration) -> bool {
        when >= self.started_at - grace && when <= self.ended_at + grace
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Transcript {
        Transcript {
            provider: Provider::Claude,
            session_id: "abc".into(),
            source_path: PathBuf::from("/tmp/x.jsonl"),
            cwd: Some(PathBuf::from("/repo")),
            started_at: Utc::now() - chrono::Duration::minutes(10),
            ended_at: Utc::now() - chrono::Duration::minutes(1),
            turn_count: 5,
            files_touched: vec![
                FileTouch {
                    path: PathBuf::from("/repo/a.rs"),
                    timestamp: Utc::now() - chrono::Duration::minutes(5),
                    kind: TouchKind::Write,
                },
                FileTouch {
                    path: PathBuf::from("/repo/a.rs"),
                    timestamp: Utc::now() - chrono::Duration::minutes(4),
                    kind: TouchKind::Write,
                },
                FileTouch {
                    path: PathBuf::from("/repo/b.rs"),
                    timestamp: Utc::now() - chrono::Duration::minutes(3),
                    kind: TouchKind::Read,
                },
            ],
            starting_commit: None,
        }
    }

    #[test]
    fn distinct_paths_preserves_first_seen_order() {
        let t = sample();
        let paths: Vec<_> = t.distinct_paths().into_iter().cloned().collect();
        assert_eq!(
            paths,
            vec![PathBuf::from("/repo/a.rs"), PathBuf::from("/repo/b.rs")]
        );
    }

    #[test]
    fn contains_time_respects_grace() {
        let t = sample();
        // Just before start: outside without grace, inside with.
        let just_before = t.started_at - chrono::Duration::seconds(30);
        assert!(!t.contains_time(just_before, chrono::Duration::seconds(10)));
        assert!(t.contains_time(just_before, chrono::Duration::minutes(1)));
    }

    #[test]
    fn touch_kind_weights_are_ordered() {
        assert!(TouchKind::Write.weight() > TouchKind::Read.weight());
        assert_eq!(TouchKind::Write.weight(), TouchKind::Delete.weight());
    }

    #[test]
    fn provider_wire_name_is_lowercase() {
        assert_eq!(Provider::Claude.as_str(), "claude");
        assert_eq!(Provider::Codex.as_str(), "codex");
        assert_eq!(Provider::OpenCode.as_str(), "opencode");
    }
}