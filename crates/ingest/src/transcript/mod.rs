// SPDX-License-Identifier: Apache-2.0
//! Transcript ingestion: discover, load, and match Claude + Codex +
//! OpenCode sessions.
//!
//! The pipeline in reverse dependency order:
//!
//! 1. [`locator`] walks disk to find candidate `.jsonl` files. (OpenCode
//!    keeps everything in a single SQLite file so it skips this stage.)
//! 2. [`claude`] / [`codex`] / [`opencode`] parse each source into a
//!    normalized [`Transcript`].
//! 3. [`matcher`] scores each `Transcript` against a commit and returns
//!    the top-N candidates with a confidence score.
//!
//! [`load_all`] is the high-level convenience: given a repo root, find
//! every transcript that could plausibly concern that repo and return
//! them as a single pool for the matcher.

pub mod claude;
pub mod codex;
pub mod locator;
pub mod matcher;
pub mod opencode;
pub(crate) mod stream;
pub mod types;

use std::path::{Path, PathBuf};

pub use matcher::{Match, MatchParams, TranscriptMatcher};
use tracing::{debug, warn};
pub use types::{FileTouch, Provider, TouchKind, Transcript};

/// Where the transcript store lives on disk. Split out so tests can
/// point at a fixture directory without polluting `$HOME`.
#[derive(Clone, Debug)]
pub struct TranscriptRoots {
    /// Defaults to `~/.claude`.
    pub claude: Option<PathBuf>,
    /// Defaults to `~/.codex`.
    pub codex: Option<PathBuf>,
    /// Defaults to `~/.local/share/opencode`. Contains `opencode.db`.
    pub opencode_home: Option<PathBuf>,
    /// If set, only load Codex rollouts newer than this. Useful for
    /// incremental imports that already know their last-seen baseline.
    pub codex_since: Option<chrono::DateTime<chrono::Utc>>,
}

impl Default for TranscriptRoots {
    fn default() -> Self {
        Self {
            claude: default_home().map(|h| h.join(".claude")),
            codex: default_home().map(|h| h.join(".codex")),
            opencode_home: default_home().map(|h| h.join(".local/share/opencode")),
            codex_since: None,
        }
    }
}

fn default_home() -> Option<PathBuf> {
    // `HOME` on unix + macOS; `USERPROFILE` on Windows. We skip the
    // dirs/directories crates to keep the dependency surface slim.
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Discover and load every transcript that could plausibly concern the
/// repo at `repo_root`. Malformed sessions are logged and skipped; the
/// function never fails the whole import just because one jsonl is bad.
///
/// Sessions are filtered by repository identity rather than exact path
/// prefix so sibling worktrees/checkouts of the same git repo still count.
pub fn load_all(repo_root: &Path, roots: &TranscriptRoots) -> Vec<Transcript> {
    let mut out = Vec::new();

    // --- Claude: search every session and keep only those whose cwd
    //     resolves to the same underlying git repo/worktree family.
    if let Some(claude_root) = roots.claude.as_deref() {
        match locator::claude_sessions(claude_root) {
            Ok(paths) => {
                for path in paths {
                    match claude::load(&path) {
                        Ok(Some(t)) if cwd_inside(&t, repo_root) => out.push(t),
                        Ok(Some(_)) => {}
                        Ok(None) => debug!(path = %path.display(), "empty Claude session"),
                        Err(e) => warn!(
                            path = %path.display(),
                            error = %e,
                            "Claude session failed to load"
                        ),
                    }
                }
            }
            Err(e) => warn!(error = %e, root = %claude_root.display(), "Claude locator error"),
        }
    }

    // --- Codex: date-sharded, so we scan everything (optionally
    //     filtered by timestamp) and keep only sessions whose cwd falls
    //     inside repo_root.
    if let Some(codex_root) = roots.codex.as_deref() {
        match locator::codex_sessions(codex_root, roots.codex_since) {
            Ok(paths) => {
                for path in paths {
                    let t = match codex::load(&path) {
                        Ok(Some(t)) => t,
                        Ok(None) => continue,
                        Err(e) => {
                            warn!(
                                path = %path.display(),
                                error = %e,
                                "Codex session failed to load"
                            );
                            continue;
                        }
                    };
                    if cwd_inside(&t, repo_root) {
                        out.push(t);
                    }
                }
            }
            Err(e) => warn!(error = %e, root = %codex_root.display(), "Codex locator error"),
        }
    }

    // --- OpenCode: single SQLite file. The loader applies its own cwd
    //     filter internally (SQL-side is a single-table scan), so we just
    //     hand it the repo root and splice the results in.
    if let Some(opencode_home) = roots.opencode_home.as_deref() {
        out.extend(opencode::load(opencode_home, repo_root));
    }

    out
}

fn cwd_inside(t: &Transcript, repo_root: &Path) -> bool {
    match t.cwd.as_ref() {
        Some(cwd) => repo_matches_checkout(cwd, repo_root),
        None => false,
    }
}

pub(crate) fn repo_matches_checkout(candidate: &Path, repo_root: &Path) -> bool {
    match (repo_common_dir(candidate), repo_common_dir(repo_root)) {
        (Some(candidate_common), Some(repo_common)) => candidate_common == repo_common,
        _ => candidate.starts_with(repo_root),
    }
}

pub(crate) fn repo_workdir(path: &Path) -> Option<PathBuf> {
    let repo = gix::discover(path).ok()?;
    let workdir = repo.workdir()?;
    canonicalize_fallback(workdir)
}

fn repo_common_dir(path: &Path) -> Option<PathBuf> {
    let repo = gix::discover(path).ok()?;
    canonicalize_fallback(repo.common_dir())
}

fn canonicalize_fallback(path: &Path) -> Option<PathBuf> {
    path.canonicalize()
        .ok()
        .or_else(|| Some(path.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn load_all_pulls_claude_and_codex_for_a_repo() {
        // Fixture: one Claude session under the expected slug dir, one
        // Codex rollout whose cwd points at the repo, and one rogue
        // Codex rollout for an unrelated directory that should be
        // filtered out.
        let tmp = TempDir::new().unwrap();
        let repo_root = PathBuf::from("/repo");

        let claude_root = tmp.path().join("claude");
        let claude_dir = claude_root
            .join("projects")
            .join(locator::claude_slug_for(&repo_root));
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(
            claude_dir.join("a.jsonl"),
            r#"{"type":"user","sessionId":"C","cwd":"/repo","timestamp":"2026-04-21T10:00:00Z"}"#,
        )
        .unwrap();

        let codex_root = tmp.path().join("codex");
        let shard = codex_root.join("sessions/2026/04/21");
        std::fs::create_dir_all(&shard).unwrap();
        std::fs::write(
            shard.join("rollout-2026-04-21T10-00-00-good.jsonl"),
            r#"{"timestamp":"2026-04-21T10:00:00Z","type":"session_meta","payload":{"id":"X","cwd":"/repo/crates"}}"#,
        )
        .unwrap();
        std::fs::write(
            shard.join("rollout-2026-04-21T11-00-00-bad.jsonl"),
            r#"{"timestamp":"2026-04-21T11:00:00Z","type":"session_meta","payload":{"id":"Y","cwd":"/somewhere/else"}}"#,
        )
        .unwrap();

        let roots = TranscriptRoots {
            claude: Some(claude_root),
            codex: Some(codex_root),
            opencode_home: None,
            codex_since: None,
        };
        let mut got = load_all(&repo_root, &roots);
        got.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        let ids: Vec<_> = got.iter().map(|t| t.session_id.clone()).collect();
        assert_eq!(ids, vec!["C".to_string(), "X".to_string()]);
    }

    #[test]
    fn load_all_rejects_parent_directory_codex_sessions() {
        let tmp = TempDir::new().unwrap();
        let repo_root = PathBuf::from("/repo/project");
        let codex_root = tmp.path().join("codex");
        let shard = codex_root.join("sessions/2026/04/21");
        std::fs::create_dir_all(&shard).unwrap();
        std::fs::write(
            shard.join("rollout-parent.jsonl"),
            r#"{"timestamp":"2026-04-21T10:00:00Z","type":"session_meta","payload":{"id":"P","cwd":"/repo"}}"#,
        )
        .unwrap();
        std::fs::write(
            shard.join("rollout-child.jsonl"),
            r#"{"timestamp":"2026-04-21T10:00:00Z","type":"session_meta","payload":{"id":"C","cwd":"/repo/project/crates"}}"#,
        )
        .unwrap();

        let roots = TranscriptRoots {
            claude: None,
            codex: Some(codex_root),
            opencode_home: None,
            codex_since: None,
        };
        let got = load_all(&repo_root, &roots);
        let ids: Vec<_> = got.iter().map(|t| t.session_id.as_str()).collect();
        assert_eq!(ids, vec!["C"]);
    }

    #[test]
    fn load_all_pulls_opencode_too() {
        // Only OpenCode configured — verifies the SQLite provider
        // participates in the combined pool.
        let tmp = TempDir::new().unwrap();
        let repo_root = PathBuf::from("/repo");

        let opencode_home = tmp.path().join("opencode");
        std::fs::create_dir_all(&opencode_home).unwrap();
        let db_path = opencode_home.join(opencode::DB_RELATIVE_PATH);
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, \
             directory TEXT NOT NULL, title TEXT NOT NULL, version TEXT NOT NULL, \
             time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, \
             time_created INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, \
             session_id TEXT NOT NULL, time_created INTEGER NOT NULL, data TEXT NOT NULL);
             INSERT INTO session VALUES ('OC', 'p', '/repo', 't', 'v', 1000, 2000);",
        )
        .unwrap();
        drop(conn);

        let roots = TranscriptRoots {
            claude: None,
            codex: None,
            opencode_home: Some(opencode_home),
            codex_since: None,
        };
        let got = load_all(&repo_root, &roots);
        let ids: Vec<_> = got.iter().map(|t| t.session_id.clone()).collect();
        assert_eq!(ids, vec!["OC".to_string()]);
    }

    #[test]
    fn repo_matches_checkout_accepts_linked_worktrees_of_same_repo() {
        let tmp = TempDir::new().unwrap();
        let main = tmp.path().join("main");
        std::fs::create_dir_all(&main).unwrap();

        let init = Command::new("git")
            .args(["init"])
            .arg(&main)
            .output()
            .expect("git init");
        assert!(init.status.success(), "git init failed: {init:?}");

        let config_name = Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(["config", "user.name", "Test User"])
            .output()
            .expect("git config user.name");
        assert!(
            config_name.status.success(),
            "git config failed: {config_name:?}"
        );
        let config_email = Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .expect("git config user.email");
        assert!(
            config_email.status.success(),
            "git config failed: {config_email:?}"
        );

        std::fs::write(main.join("README.md"), "hello\n").unwrap();
        let add = Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(["add", "README.md"])
            .output()
            .expect("git add");
        assert!(add.status.success(), "git add failed: {add:?}");
        let commit = Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(["commit", "-m", "init"])
            .output()
            .expect("git commit");
        assert!(commit.status.success(), "git commit failed: {commit:?}");

        let worktree = tmp.path().join("wt-a");
        let wt = Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(["worktree", "add"])
            .arg(&worktree)
            .args(["-b", "wt-a"])
            .output()
            .expect("git worktree add");
        assert!(wt.status.success(), "git worktree add failed: {wt:?}");

        assert!(repo_matches_checkout(&worktree, &main));
        assert!(repo_matches_checkout(&main, &worktree));
    }
}
