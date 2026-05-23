// SPDX-License-Identifier: Apache-2.0
//! Discover agent transcript files on disk.
//!
//! Both providers follow stable layouts we can enumerate without
//! parsing every file up front:
//!
//! - **Claude Code**: `~/.claude/projects/<slug>/<uuid>.jsonl` —
//!   `<slug>` is the working directory with `/` replaced by `-` (leading
//!   slash becomes a leading `-`). A single repo path maps to exactly
//!   one slug directory.
//! - **Codex Desktop**: `~/.codex/sessions/<YYYY>/<MM>/<DD>/rollout-<ts>-<uuid>.jsonl`
//!   — date-sharded, not cwd-keyed. To match a given repo we have to
//!   read each file's `session_meta` cwd. Older rollouts can also be
//!   moved flat into `~/.codex/archived_sessions/rollout-<ts>-<uuid>.jsonl`;
//!   same file format, so we enumerate both locations.
//!
//! For the v1 importer we return *candidate* paths and let the loader
//! decide whether a session is relevant (via cwd match in the matcher).
//! That's an extra read per Codex session but keeps the locator
//! stateless and cheap to reason about.

use std::path::{Path, PathBuf};

/// Convert a working-directory path into Claude's slug form. Only used
/// for narrowing the Claude search — the loader still reads `cwd` from
/// each event to be sure.
///
/// Claude replaces both path separators *and* dots (so dotted-dirs like
/// `.claude/worktrees` don't confuse the slug encoder). Both get folded
/// into `-`, which means a cwd containing `/.claude` produces a `--claude`
/// double-dash in the slug.
///
/// `/Users/foo/dev/heddle` → `-Users-foo-dev-heddle`
/// `/Users/foo/dev/heddle/.claude/worktrees/x` → `-Users-foo-dev-heddle--claude-worktrees-x`
pub fn claude_slug_for(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    s.chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect()
}

/// Enumerate every `.jsonl` under `~/.claude/projects/<slug>` (or the
/// override given). Returns an empty vec if the dir doesn't exist —
/// absence of transcripts is normal (first-time users, other CLIs).
pub fn claude_sessions_for(claude_root: &Path, cwd: &Path) -> std::io::Result<Vec<PathBuf>> {
    let slug = claude_slug_for(cwd);
    let dir = claude_root.join("projects").join(slug);
    list_jsonl(&dir)
}

/// Enumerate every Claude session under `~/.claude/projects`.
///
/// Used when callers need to match across sibling worktrees/checkouts of the
/// same repo rather than a single exact slug directory.
pub fn claude_sessions(claude_root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk_jsonl_recursive(&claude_root.join("projects"), &mut out)?;
    out.sort();
    Ok(out)
}

/// Enumerate every Codex rollout under `~/.codex/sessions` and
/// `~/.codex/archived_sessions`. No cwd pre-filter — the caller should
/// load and match by `session_meta.cwd`.
///
/// An optional `since` cut-off trims files whose filename-encoded start
/// timestamp predates it, so an importer with a known baseline doesn't
/// have to open every rollout in history.
pub fn codex_sessions(
    codex_root: &Path,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk_jsonl_recursive(&codex_root.join("sessions"), &mut out)?;
    walk_jsonl_recursive(&codex_root.join("archived_sessions"), &mut out)?;
    if let Some(cutoff) = since {
        out.retain(|p| filename_timestamp(p).is_none_or(|ts| ts >= cutoff));
    }
    out.sort();
    Ok(out)
}

fn list_jsonl(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

fn walk_jsonl_recursive(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            walk_jsonl_recursive(&path, out)?;
        } else if ft.is_file() && path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
    Ok(())
}

/// Extract the ISO-like timestamp Codex encodes in the filename. Format:
/// `rollout-<YYYY>-<MM>-<DD>T<HH>-<MM>-<SS>-<uuid>.jsonl`. The dashes in
/// the time component trip up `chrono`'s ISO parser, so we swap them
/// back to colons before parsing.
fn filename_timestamp(path: &Path) -> Option<chrono::DateTime<chrono::Utc>> {
    let stem = path.file_stem()?.to_str()?;
    let ts_part = stem.strip_prefix("rollout-")?;
    // Expect `YYYY-MM-DDTHH-MM-SS-<uuid>`. Split off the uuid at the
    // last dash-group boundary (uuid itself contains dashes), then
    // reformat the time slashes.
    // The full timestamp is the first 19 chars — `YYYY-MM-DDTHH-MM-SS`.
    if ts_part.len() < 19 {
        return None;
    }
    let (raw_ts, _) = ts_part.split_at(19);
    // `raw_ts` looks like `2026-03-20T23-08-42`; convert the last two
    // hyphens into colons so chrono accepts it.
    let mut chars: Vec<char> = raw_ts.chars().collect();
    // positions 13 and 16 are the hyphens that should be `:`
    if chars.get(13) == Some(&'-') {
        chars[13] = ':';
    }
    if chars.get(16) == Some(&'-') {
        chars[16] = ':';
    }
    let iso = format!("{}Z", chars.iter().collect::<String>());
    chrono::DateTime::parse_from_rfc3339(&iso)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn slug_replaces_path_separators() {
        assert_eq!(
            claude_slug_for(Path::new("/Users/foo/dev/heddle")),
            "-Users-foo-dev-heddle"
        );
    }

    #[test]
    fn slug_folds_dots_into_dashes_so_dotted_dirs_resolve() {
        // This is the real-world worktree case: `.claude/worktrees/...`
        // on disk collapses to `--claude-worktrees-...` in Claude's slug.
        assert_eq!(
            claude_slug_for(Path::new("/Users/foo/dev/heddle/.claude/worktrees/xyz")),
            "-Users-foo-dev-heddle--claude-worktrees-xyz"
        );
    }

    #[test]
    fn claude_sessions_returns_empty_when_dir_missing() {
        let tmp = TempDir::new().unwrap();
        let got = claude_sessions_for(tmp.path(), Path::new("/no/such/cwd")).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn claude_sessions_finds_jsonl_and_skips_others() {
        let tmp = TempDir::new().unwrap();
        let cwd = Path::new("/repo");
        let dir = tmp.path().join("projects").join(claude_slug_for(cwd));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.jsonl"), "x").unwrap();
        std::fs::write(dir.join("b.jsonl"), "y").unwrap();
        std::fs::write(dir.join("notes.md"), "ignored").unwrap();

        let got = claude_sessions_for(tmp.path(), cwd).unwrap();
        let names: Vec<_> = got
            .iter()
            .map(|p| p.file_name().unwrap().to_owned())
            .collect();
        assert_eq!(names, vec!["a.jsonl", "b.jsonl"]);
    }

    #[test]
    fn claude_sessions_walks_all_slug_dirs() {
        let tmp = TempDir::new().unwrap();
        let projects = tmp.path().join("projects");
        std::fs::create_dir_all(projects.join("slug-a")).unwrap();
        std::fs::create_dir_all(projects.join("slug-b")).unwrap();
        std::fs::write(projects.join("slug-a/a.jsonl"), "x").unwrap();
        std::fs::write(projects.join("slug-b/b.jsonl"), "y").unwrap();

        let got = claude_sessions(tmp.path()).unwrap();
        let names: Vec<_> = got
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.jsonl", "b.jsonl"]);
    }

    #[test]
    fn codex_sessions_walks_date_shards_and_sorts() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions").join("2026").join("03");
        std::fs::create_dir_all(root.join("20")).unwrap();
        std::fs::create_dir_all(root.join("21")).unwrap();
        std::fs::write(root.join("20/rollout-2026-03-20T23-08-42-abc.jsonl"), "x").unwrap();
        std::fs::write(root.join("21/rollout-2026-03-21T10-00-00-def.jsonl"), "y").unwrap();

        // Archived rollouts live flat under `archived_sessions/` (not
        // date-sharded) but use the same filename format; they must
        // show up in the same candidate list.
        let archived = tmp.path().join("archived_sessions");
        std::fs::create_dir_all(&archived).unwrap();
        std::fs::write(archived.join("rollout-2026-03-15T09-00-00-ghi.jsonl"), "z").unwrap();

        let got = codex_sessions(tmp.path(), None).unwrap();
        assert_eq!(got.len(), 3);
        // Lexicographic sort happens to equal chronological for this naming.
        let names: Vec<_> = got
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "rollout-2026-03-15T09-00-00-ghi.jsonl",
                "rollout-2026-03-20T23-08-42-abc.jsonl",
                "rollout-2026-03-21T10-00-00-def.jsonl",
            ]
        );
        assert!(
            got.iter()
                .any(|p| p.to_string_lossy().contains("archived_sessions")),
            "expected archived rollout to appear in candidate list: {got:?}"
        );
    }

    #[test]
    fn codex_since_filter_trims_older_files() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions").join("2026").join("03");
        std::fs::create_dir_all(root.join("20")).unwrap();
        std::fs::create_dir_all(root.join("21")).unwrap();
        std::fs::write(root.join("20/rollout-2026-03-20T10-00-00-old.jsonl"), "").unwrap();
        std::fs::write(root.join("21/rollout-2026-03-21T10-00-00-new.jsonl"), "").unwrap();

        let cutoff = chrono::DateTime::parse_from_rfc3339("2026-03-21T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let got = codex_sessions(tmp.path(), Some(cutoff)).unwrap();
        assert_eq!(got.len(), 1);
        assert!(got[0].to_string_lossy().contains("new.jsonl"));
    }

    #[test]
    fn filename_timestamp_handles_the_rollout_format() {
        let ts = filename_timestamp(Path::new(
            "/x/rollout-2026-03-20T23-08-42-019d0e94-c06c-7f92-9567-bda9e01f388f.jsonl",
        ))
        .unwrap();
        assert_eq!(ts.to_rfc3339(), "2026-03-20T23:08:42+00:00");
    }
}
