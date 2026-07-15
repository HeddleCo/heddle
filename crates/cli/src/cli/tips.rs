// SPDX-License-Identifier: Apache-2.0
//! Discoverability tips (A17).
//!
//! After a successful verb, the CLI may emit a one-line tip nudging the
//! user toward a more powerful affordance. Tips are:
//!
//! - **stderr only**: piping `heddle <verb>` to other tools never includes
//!   tips.
//! - **never in `--output json`**: scripted consumers don't get advisory output.
//! - **once per session per repo**: a session marker file at
//!   `~/.heddle/session/<repo-id>/tips-shown.toml` records which tips
//!   have been shown so we don't nag.
//! - **per-repo permanently suppressible** via `[ui.tips] enabled = false`
//!   in `.heddle/config.toml` (or `[ui.tips.suppress] keys = [...]` to
//!   suppress individual tips).

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tip {
    /// "tip: Git Overlay source history stays in direct Git commands."
    /// Emitted after a successful capture.
    CheckpointAfterCapture,
    /// "tip: `heddle query` searches saved change history."
    /// Emitted after the first heavy `heddle log` view.
    QueryFromLog,
    /// "tip: `heddle agent serve` runs a local daemon for tight loops."
    /// Emitted after the first state-changing verb in a fresh shell.
    AgentServeForLatency,
    /// "tip: `heddle resolve --output json` returns conflicts as structured data."
    /// Emitted on a conflicted merge.
    ConflictForStructured,
}

impl Tip {
    pub fn key(&self) -> &'static str {
        match self {
            Self::CheckpointAfterCapture => "checkpoint_after_capture",
            Self::QueryFromLog => "query_from_log",
            Self::AgentServeForLatency => "agent_serve_for_latency",
            Self::ConflictForStructured => "conflict_for_structured",
        }
    }

    pub fn message(&self) -> &'static str {
        match self {
            Self::CheckpointAfterCapture => {
                "tip: in Git Overlay, run `heddle commit` when the captured state is ready"
            }
            Self::QueryFromLog => "tip: `heddle query` searches saved change history",
            Self::AgentServeForLatency => {
                "tip: `heddle agent serve` runs a local daemon that cuts per-command latency for agent loops"
            }
            Self::ConflictForStructured => {
                "tip: `heddle resolve --output json` returns conflicts as structured data agents can resolve programmatically"
            }
        }
    }
}

/// Identify the per-repo session marker directory. Hashes the canonical
/// repo root path so distinct worktrees of the same repo don't collide.
pub fn session_marker_dir(repo_root: &std::path::Path) -> PathBuf {
    let canonical = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let hash = blake3::hash(canonical.to_string_lossy().as_bytes());
    let id = hex::encode(&hash.as_bytes()[..8]);
    let home = dirs_home().unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".heddle").join("session").join(id)
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn marker_file(repo_root: &std::path::Path) -> PathBuf {
    session_marker_dir(repo_root).join("tips-shown.toml")
}

/// Returns true if the tip has already been shown for this repo+session.
///
/// Resolves the marker path via `HOME`. The path-taking variant is
/// [`already_shown_at`]; tests use that to avoid touching process env.
fn already_shown(repo_root: &std::path::Path, tip: Tip) -> bool {
    already_shown_at(&marker_file(repo_root), tip)
}

fn record_shown(repo_root: &std::path::Path, tip: Tip) -> std::io::Result<()> {
    record_shown_at(&marker_file(repo_root), tip)
}

/// Path-taking primitive: read the marker file at `path` and check
/// whether `tip`'s key appears. Returns `false` when the file is
/// missing or unreadable. Pure I/O — no env access.
fn already_shown_at(path: &std::path::Path, tip: Tip) -> bool {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    raw.lines()
        .any(|line| line.split_whitespace().next() == Some(tip.key()))
}

/// Path-taking primitive: append `tip` to the marker file at `path`.
fn record_shown_at(path: &std::path::Path, tip: Tip) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    use std::io::Write;
    let line = format!("{} {}\n", tip.key(), unix_secs());
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(line.as_bytes())
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Emit a tip on stderr if it hasn't been shown yet for this repo and the
/// caller hasn't suppressed tips. The `as_json` argument should be `true`
/// when the verb is rendering JSON — tips are skipped there to keep
/// scripted output clean. The `quiet` argument reflects the global
/// `--quiet` flag and suppresses nonessential discoverability copy.
pub fn maybe_emit(
    repo_root: &std::path::Path,
    cfg: Option<&repo::RepoConfig>,
    tip: Tip,
    as_json: bool,
    quiet: bool,
) {
    if as_json || quiet {
        return;
    }
    if let Some(cfg) = cfg
        && !cfg_tips_enabled(cfg)
    {
        return;
    }
    if let Some(cfg) = cfg
        && cfg_tip_suppressed(cfg, tip)
    {
        return;
    }
    if already_shown(repo_root, tip) {
        return;
    }
    eprintln!("{}", tip.message());
    let _ = record_shown(repo_root, tip);
}

fn cfg_tips_enabled(_cfg: &repo::RepoConfig) -> bool {
    // The repo config doesn't yet carry a `[ui.tips]` section. When we
    // add it (W2 follow-up), wire `cfg.ui.tips.enabled` here. For now,
    // tips are on by default with the per-tip session-marker check
    // providing the not-too-noisy bound.
    true
}

fn cfg_tip_suppressed(_cfg: &repo::RepoConfig, _tip: Tip) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn tip_keys_are_unique_and_stable() {
        let keys = [
            Tip::CheckpointAfterCapture.key(),
            Tip::QueryFromLog.key(),
            Tip::AgentServeForLatency.key(),
            Tip::ConflictForStructured.key(),
        ];
        let unique: std::collections::HashSet<_> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len(), "duplicate tip keys");
    }

    #[test]
    fn already_shown_after_record_at_path() {
        // Test the path-taking primitives directly. The env-resolving
        // wrappers (`already_shown` / `record_shown`) read `HOME`,
        // which is process-global and races with parallel tests; this
        // covers the same logic without touching the environment.
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("tips-shown.toml");
        assert!(!already_shown_at(&path, Tip::CheckpointAfterCapture));
        record_shown_at(&path, Tip::CheckpointAfterCapture).unwrap();
        assert!(already_shown_at(&path, Tip::CheckpointAfterCapture));
    }

    #[test]
    fn record_shown_at_appends_distinct_tips() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("tips-shown.toml");
        record_shown_at(&path, Tip::CheckpointAfterCapture).unwrap();
        record_shown_at(&path, Tip::QueryFromLog).unwrap();
        assert!(already_shown_at(&path, Tip::CheckpointAfterCapture));
        assert!(already_shown_at(&path, Tip::QueryFromLog));
        assert!(!already_shown_at(&path, Tip::AgentServeForLatency));
    }

    #[test]
    fn already_shown_at_missing_file_is_not_shown() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("does-not-exist.toml");
        assert!(!already_shown_at(&path, Tip::CheckpointAfterCapture));
    }

    #[test]
    fn maybe_emit_is_noop_in_json_mode() {
        // Just exercises the gate — actual eprintln capture is fragile
        // across platforms. This guards the early-return branch.
        let temp = TempDir::new().unwrap();
        maybe_emit(temp.path(), None, Tip::CheckpointAfterCapture, true, false);
    }

    #[test]
    fn maybe_emit_is_noop_in_quiet_mode() {
        // Same as JSON mode: quiet suppresses nonessential tips.
        let temp = TempDir::new().unwrap();
        maybe_emit(temp.path(), None, Tip::CheckpointAfterCapture, false, true);
    }
}
