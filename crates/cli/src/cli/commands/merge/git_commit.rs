// SPDX-License-Identifier: Apache-2.0
//! Optional git-commit coordination for `heddle merge --git-commit`.
//!
//! Closes the heddle-vs-git divergence at merge time: when the user
//! opts in, after a successful (non-preview, non-conflict) heddle merge
//! we also write a git commit on top of HEAD, staging the paths the
//! merge introduced. The default (`--git-commit` not set) is preserved
//! — heddle state advances and git is unaware.

use std::{path::Path, process::Command};

use anyhow::{Result, anyhow};
use objects::object::Attribution;
use serde::Serialize;

/// Outcome of `--git-commit --preview` — what *would* be committed if
/// the merge ran for real.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct GitCommitPreview {
    pub message: String,
    pub files: Vec<String>,
}

/// Outcome of a real `--git-commit` write.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct GitCommitInfo {
    pub sha: String,
    pub message: String,
}

/// Reasons the `--git-commit` request can't proceed. Surfaced via the
/// merge output's `blockers` list with `status: "blocked"`, matching
/// the schema settled by item 1.1.
#[derive(Debug)]
pub(super) struct GitCommitBlocked {
    pub blockers: Vec<String>,
}

impl std::fmt::Display for GitCommitBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "git commit blocked: {}", self.blockers.join("; "))
    }
}

impl std::error::Error for GitCommitBlocked {}

/// Validate that git is in a state where we can safely write a merge
/// commit. The merge has already enforced a clean *heddle* worktree;
/// here we additionally enforce that the only uncommitted git changes
/// are the ones the merge just produced (or, in preview mode, the ones
/// the merge would touch).
///
/// `expected_paths` is the set of paths the merge will/did write — any
/// other uncommitted git change is "unrelated" and blocks the
/// `--git-commit` flow rather than getting silently swept up.
pub(super) fn validate_git_state(
    repo_root: &Path,
    expected_paths: &[String],
) -> std::result::Result<(), GitCommitBlocked> {
    let mut blockers = Vec::new();

    if !repo_root.join(".git").exists() {
        blockers.push(format!(
            "no git repository at {} (--git-commit requires a git overlay)",
            repo_root.display()
        ));
        return Err(GitCommitBlocked { blockers });
    }

    // Detached HEAD blocks the commit — a merge commit on a detached
    // HEAD would be unreachable once HEAD moves.
    let head_check = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["symbolic-ref", "--quiet", "HEAD"])
        .output();
    match head_check {
        Ok(out) if !out.status.success() => {
            blockers.push("git HEAD is detached (--git-commit requires an attached branch)".into());
        }
        Err(err) => {
            blockers.push(format!("failed to inspect git HEAD: {err}"));
            return Err(GitCommitBlocked { blockers });
        }
        _ => {}
    }

    // `git status --porcelain -z` for the unrelated-changes check.
    // Plain `--porcelain` quotes/escapes pathnames with spaces or
    // non-ASCII bytes (`"a b.txt"`, `"a\tb.txt"`); the `-z` form
    // emits NUL-separated raw bytes with no quoting, which is the
    // only safe way to compare against `expected_paths`. We tolerate
    // dirt on the expected paths (the merge writes there) and reject
    // everything else.
    let status = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["status", "--porcelain", "-z", "--untracked-files=normal"])
        .output();
    let status = match status {
        Ok(out) if out.status.success() => out,
        Ok(out) => {
            blockers.push(format!(
                "git status failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
            return Err(GitCommitBlocked { blockers });
        }
        Err(err) => {
            blockers.push(format!("failed to run git status: {err}"));
            return Err(GitCommitBlocked { blockers });
        }
    };

    let expected: std::collections::HashSet<&str> =
        expected_paths.iter().map(|p| p.as_str()).collect();
    let unrelated = parse_porcelain_z_unrelated(&status.stdout, &expected);

    if !unrelated.is_empty() {
        // Cap the rendered list — the user gets the count and a few
        // examples; the full set lives in the workspace anyway.
        // Per-path: if the path looks like common noise (`.DS_Store`,
        // `xcuserdata/...`, editor swap files), append an inline
        // `.heddleignore` hint so the user can fix the root cause in
        // one edit instead of guessing the right glob.
        let preview: Vec<String> = unrelated
            .iter()
            .take(5)
            .map(|path| {
                match super::super::heddleignore_defaults::noise_hint_for(std::path::Path::new(
                    path,
                )) {
                    Some(hint) => format!("{path} {}", hint.render_inline()),
                    None => path.clone(),
                }
            })
            .collect();
        let suffix = if unrelated.len() > preview.len() {
            format!(" (+{} more)", unrelated.len() - preview.len())
        } else {
            String::new()
        };
        blockers.push(format!(
            "{} unrelated uncommitted git change(s) outside the merge: {}{}",
            unrelated.len(),
            preview.join(", "),
            suffix
        ));
    }

    if blockers.is_empty() {
        Ok(())
    } else {
        Err(GitCommitBlocked { blockers })
    }
}

/// Parse `git status --porcelain -z` output and return the set of paths
/// that aren't in `expected`. The `-z` format is the only one that's
/// safe for paths with spaces, tabs, or non-ASCII bytes — plain
/// `--porcelain` C-quotes them, which would never compare equal to the
/// raw `expected_paths` strings the merge tracks.
///
/// Format reference (`git status --porcelain --help`):
/// - Each record starts with two status chars + space (`XY `).
/// - For non-rename/copy records: `XY <PATH>\0`.
/// - For renames/copies (status starts with `R` or `C`):
///   `XY <PATH>\0<ORIG_PATH>\0` — the new path is in the main record,
///   then the original path is a separate NUL-terminated trailer. We
///   only care about the new path for the unrelated check (the merge
///   would have produced a write at the new path).
fn parse_porcelain_z_unrelated(
    raw: &[u8],
    expected: &std::collections::HashSet<&str>,
) -> Vec<String> {
    let mut unrelated: Vec<String> = Vec::new();
    // Split on NUL into records, then walk pair-wise where the status
    // code dictates whether the next record is the rename/copy origin.
    let records: Vec<&[u8]> = raw.split(|b| *b == 0).filter(|r| !r.is_empty()).collect();
    let mut i = 0;
    while i < records.len() {
        let rec = records[i];
        if rec.len() < 4 {
            i += 1;
            continue;
        }
        // First two bytes are the index/worktree status flags. `R` or
        // `C` in either column means a rename/copy record, which is
        // followed by a separate origin-path record.
        let xy = &rec[..2];
        let is_rename_or_copy = xy.iter().any(|c| matches!(*c, b'R' | b'C'));
        let path_bytes = &rec[3..];
        // Lossy decode is fine: if the path isn't valid UTF-8 we can't
        // match it against `expected_paths` (which are Rust strings)
        // anyway, and reporting the lossy form in the blocker message
        // is still useful diagnostic output.
        let path = String::from_utf8_lossy(path_bytes).into_owned();
        if !expected.contains(path.as_str()) {
            unrelated.push(path);
        }
        if is_rename_or_copy {
            // Skip the origin-path trailer — we already captured the
            // new path, and the rename's origin is implied dirt that
            // the merge didn't claim either; but reporting both would
            // double-count, so we drop it.
            i += 2;
        } else {
            i += 1;
        }
    }
    unrelated
}

/// Build the commit message. Body includes the heddle merge state ID
/// so post-merge audits can join git ↔ heddle. Trailers carry the
/// `Merge-State` change-id and a `Co-Authored-By` for the merge
/// attribution.
pub(super) fn build_commit_message(
    base_message: &str,
    merge_state_id: &str,
    attribution: &Attribution,
) -> String {
    let subject = base_message.lines().next().unwrap_or(base_message).trim();
    let mut out = String::new();
    out.push_str(subject);
    out.push_str("\n\n");
    out.push_str(&format!("Heddle merge state: {merge_state_id}\n"));
    out.push('\n');
    out.push_str(&format!("Merge-State: {merge_state_id}\n"));
    out.push_str(&format!(
        "Co-Authored-By: {} <{}>\n",
        attribution.principal.name, attribution.principal.email
    ));
    out
}

/// Stage the changed paths and write a single commit. Returns the
/// short SHA. We `git add` exact paths (not `-A`) so unrelated files
/// the user happened to leave on disk are never swept into the merge
/// commit. `validate_git_state` already guaranteed no unrelated dirt
/// exists at validation time, but staging precisely is still the
/// principled boundary.
pub(super) fn write_git_commit(
    repo_root: &Path,
    paths: &[String],
    message: &str,
) -> Result<GitCommitInfo> {
    if paths.is_empty() {
        return Err(anyhow!(
            "merge produced no changed paths — refusing to write an empty git commit"
        ));
    }

    // `git add -- <path...>` for each batch. `--` keeps paths that
    // start with `-` from being parsed as flags.
    let mut add_cmd = Command::new("git");
    add_cmd.arg("-C").arg(repo_root).args(["add", "--"]);
    for path in paths {
        add_cmd.arg(path);
    }
    let add = add_cmd
        .output()
        .map_err(|err| anyhow!("git add failed: {err}"))?;
    if !add.status.success() {
        return Err(anyhow!(
            "git add failed: {}",
            String::from_utf8_lossy(&add.stderr).trim()
        ));
    }

    let commit = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["commit", "-m", message, "--allow-empty-message"])
        .output()
        .map_err(|err| anyhow!("git commit failed: {err}"))?;
    if !commit.status.success() {
        return Err(anyhow!(
            "git commit failed: {}",
            String::from_utf8_lossy(&commit.stderr).trim()
        ));
    }

    let rev = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .map_err(|err| anyhow!("git rev-parse failed: {err}"))?;
    if !rev.status.success() {
        return Err(anyhow!(
            "git rev-parse HEAD failed: {}",
            String::from_utf8_lossy(&rev.stderr).trim()
        ));
    }
    let sha = String::from_utf8_lossy(&rev.stdout).trim().to_string();

    Ok(GitCommitInfo {
        sha,
        message: message.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use objects::object::Principal;

    use super::*;

    #[test]
    fn build_commit_message_has_merge_state_trailer_and_coauthor() {
        let attribution = Attribution::human(Principal::new("Ada Lovelace", "ada@example.com"));
        let msg = build_commit_message("Merge thread 'feature'", "abcd1234", &attribution);
        assert!(msg.starts_with("Merge thread 'feature'\n\n"));
        assert!(msg.contains("Heddle merge state: abcd1234\n"));
        assert!(msg.contains("\nMerge-State: abcd1234\n"));
        assert!(msg.contains("Co-Authored-By: Ada Lovelace <ada@example.com>\n"));
    }

    /// Regression for codex feedback on PR #62: plain `git status
    /// --porcelain` quotes paths with spaces/tabs/non-ASCII bytes —
    /// e.g. `"path with spaces.txt"` rather than the raw
    /// `path with spaces.txt`. The old parser compared the raw payload
    /// (with quotes) to the unquoted `expected_paths`, so a merge that
    /// touched such a path got rejected as having unrelated dirt.
    /// The fix uses `--porcelain -z` (NUL-separated, no quoting).
    /// This test feeds the parser a synthesized -z payload that
    /// includes a path with spaces, asserts it matches against the
    /// expected set without quoting issues, and verifies an unrelated
    /// path still surfaces as dirt.
    #[test]
    fn parse_porcelain_z_handles_paths_with_spaces() {
        // Record 1: ` M path with spaces.txt` (modified) — expected.
        // Record 2: ` M unrelated.txt` (modified) — NOT expected, must surface.
        let mut raw: Vec<u8> = Vec::new();
        raw.extend_from_slice(b" M path with spaces.txt");
        raw.push(0);
        raw.extend_from_slice(b" M unrelated.txt");
        raw.push(0);

        let expected: std::collections::HashSet<&str> =
            ["path with spaces.txt"].into_iter().collect();
        let unrelated = parse_porcelain_z_unrelated(&raw, &expected);
        assert_eq!(
            unrelated,
            vec!["unrelated.txt".to_string()],
            "the path with spaces must match against `expected` cleanly; only `unrelated.txt` should surface"
        );
    }

    /// Renames/copies have a follow-up origin path in the -z stream
    /// (`XY <new>\0<orig>\0`). The parser must only consider the new
    /// path for the expected-set check, and must skip the origin
    /// trailer rather than double-reporting it.
    #[test]
    fn parse_porcelain_z_skips_rename_origin_trailer() {
        let mut raw: Vec<u8> = Vec::new();
        // R rename: new path is `dst path.txt`, origin is `src.txt`.
        raw.extend_from_slice(b"R  dst path.txt");
        raw.push(0);
        raw.extend_from_slice(b"src.txt");
        raw.push(0);
        // Followed by a normal modified file that's expected.
        raw.extend_from_slice(b" M expected.txt");
        raw.push(0);

        let expected: std::collections::HashSet<&str> =
            ["dst path.txt", "expected.txt"].into_iter().collect();
        let unrelated = parse_porcelain_z_unrelated(&raw, &expected);
        assert!(
            unrelated.is_empty(),
            "rename's new-path side is expected and origin is the trailer — nothing should surface. got: {unrelated:?}"
        );
    }

    /// Counter-test: paths NOT in `expected` must surface as unrelated,
    /// including paths with spaces. Confirms the parser doesn't silently
    /// drop dirt because the path happens to be quoted under the old
    /// porcelain v1 format.
    #[test]
    fn parse_porcelain_z_reports_unrelated_paths_with_spaces() {
        let mut raw: Vec<u8> = Vec::new();
        raw.extend_from_slice(b" M weird path.txt");
        raw.push(0);

        let expected: std::collections::HashSet<&str> = ["other.txt"].into_iter().collect();
        let unrelated = parse_porcelain_z_unrelated(&raw, &expected);
        assert_eq!(unrelated, vec!["weird path.txt".to_string()]);
    }

    /// End-to-end check: spin up a real git repo, create a tracked file
    /// whose path contains a space, and confirm `validate_git_state`
    /// accepts it as expected-dirt. Pre-fix, the C-quoted form
    /// (`"path with spaces.txt"`) wouldn't match the raw expected
    /// string and the merge would be rejected as having unrelated
    /// changes.
    #[test]
    fn validate_git_state_accepts_path_with_spaces_as_expected() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path();

        // Initialise a git repo with an attached HEAD on a branch.
        let _ = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["init", "--initial-branch=main"])
            .output()
            .expect("git init");
        // Configure identity so commit works (some CIs require it).
        let _ = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["config", "user.email", "test@example.com"])
            .output();
        let _ = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["config", "user.name", "Test"])
            .output();
        // Seed an initial commit so HEAD is symbolic on `main`.
        std::fs::write(root.join("seed.txt"), b"seed").unwrap();
        let _ = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["add", "seed.txt"])
            .output();
        let _ = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["commit", "-m", "seed"])
            .output();

        // Now create the path-with-spaces file as the only working-tree
        // change. validate_git_state must accept it since it's listed
        // in expected_paths.
        let weird = "path with spaces.txt";
        std::fs::write(root.join(weird), b"hello").unwrap();

        let expected_paths = vec![weird.to_string()];
        let result = validate_git_state(root, &expected_paths);
        assert!(
            result.is_ok(),
            "validate_git_state must accept a path with spaces when it's in expected_paths, got: {:?}",
            result.err()
        );
    }

    #[test]
    fn build_commit_message_uses_only_first_subject_line() {
        let attribution = Attribution::human(Principal::new("Test", "test@example.com"));
        let msg = build_commit_message(
            "Merge thread 'x'\n\nlonger body\nthat we drop",
            "deadbeef",
            &attribution,
        );
        // Subject line should be just the first line.
        assert!(msg.starts_with("Merge thread 'x'\n\n"));
        assert!(!msg.contains("longer body"));
    }
}
