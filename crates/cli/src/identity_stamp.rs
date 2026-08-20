// SPDX-License-Identifier: Apache-2.0
//! Fast-path `heddle integration stamp` — parse stdin, atomic-rename cursor.
//!
//! Dispatched before tokio / clap / repo open. Budget: the user cannot feel
//! the hook. No relay, no JSONL, no session.get, no disk glob.

use std::{
    env, io,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use heddle_core::{
    IdentityCursor, cursor_patch_from_stdin, expire_identity_cursor, stamp_event_expires,
    stamp_harness_name, stamp_identity_cursor,
};

/// Run the stamp fast path when argv is `integration stamp`. Returns exit code.
pub fn maybe_run_fast_path() -> Option<i32> {
    let args: Vec<String> = env::args().skip(1).collect();
    parse_stamp_args(&args).map(|parsed| match run_stamp(&parsed) {
        Ok(()) => 0,
        Err(_) => 0, // hooks must not fail the tool; next capture retries
    })
}

#[derive(Debug)]
struct StampArgs {
    repo: Option<PathBuf>,
    harness: String,
    chain: Option<String>,
    expire: bool,
}

fn parse_stamp_args(args: &[String]) -> Option<StampArgs> {
    let mut repo = None;
    let mut chain = None;
    let mut expire = false;
    let mut positional = Vec::new();
    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        if arg == "--repo" {
            idx += 1;
            repo = args.get(idx).map(PathBuf::from);
        } else if let Some(value) = arg.strip_prefix("--repo=") {
            repo = Some(PathBuf::from(value));
        } else if arg == "--chain" {
            idx += 1;
            chain = args.get(idx).cloned();
        } else if let Some(value) = arg.strip_prefix("--chain=") {
            chain = Some(value.to_string());
        } else if arg == "--expire" {
            expire = true;
        } else if arg == "--" {
            positional.extend(args[idx + 1..].iter().cloned());
            break;
        } else {
            positional.push(arg.clone());
        }
        idx += 1;
    }
    if positional.first().map(String::as_str) != Some("integration") {
        return None;
    }
    if positional.get(1).map(String::as_str) != Some("stamp") {
        return None;
    }
    let harness = positional.get(2)?.clone();
    stamp_harness_name(&harness)?;
    Some(StampArgs {
        repo,
        harness,
        chain,
        expire,
    })
}

fn run_stamp(args: &StampArgs) -> io::Result<()> {
    let mut stdin = Vec::new();
    io::stdin().read_to_end(&mut stdin)?;
    let stdin_text = String::from_utf8_lossy(&stdin);
    let Some(root) = resolve_repo_root(args.repo.as_deref()) else {
        return chain_status_line(args.chain.as_deref(), &stdin);
    };
    let _ = apply_identity_stamp(&root, &args.harness, &stdin_text, args.expire);
    chain_status_line(args.chain.as_deref(), &stdin)
}

fn apply_identity_stamp(
    repo_root: &Path,
    harness: &str,
    stdin: &str,
    expire: bool,
) -> io::Result<IdentityCursor> {
    let expires = expire
        || serde_json::from_str::<serde_json::Value>(stdin.trim())
            .ok()
            .is_some_and(|payload| stamp_event_expires(harness, &payload, None));
    if expires {
        expire_identity_cursor(repo_root)?;
        return Ok(IdentityCursor::default());
    }
    stamp_identity_cursor(repo_root, &cursor_patch_from_stdin(harness, stdin))
}

fn resolve_repo_root(explicit: Option<&Path>) -> Option<PathBuf> {
    let start = match explicit {
        Some(path) => path.to_path_buf(),
        None => env::current_dir().ok()?,
    };
    let mut dir = start.canonicalize().unwrap_or(start);
    for _ in 0..12 {
        let marker = dir.join(".heddle");
        if marker.is_dir() || marker.is_file() {
            return Some(dir);
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

fn chain_status_line(chain: Option<&str>, stdin: &[u8]) -> io::Result<()> {
    let Some(command) = chain.filter(|c| !c.trim().is_empty()) else {
        return Ok(());
    };
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    if let Some(mut child_stdin) = child.stdin.take() {
        use io::Write;
        let _ = child_stdin.write_all(stdin);
    }
    let _ = child.wait();
    Ok(())
}

/// Library entry for tests (no process stdin).
pub fn stamp_bytes(
    repo_root: &Path,
    harness: &str,
    stdin: &str,
) -> Result<IdentityCursor, io::Error> {
    apply_identity_stamp(repo_root, harness, stdin, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use heddle_core::{identity_cursor_path, read_identity_cursor};

    #[test]
    fn parse_recognises_stamp_and_rejects_relay() {
        assert!(
            parse_stamp_args(&[
                "--repo".into(),
                "/tmp/r".into(),
                "integration".into(),
                "stamp".into(),
                "claude-code".into()
            ])
            .is_some()
        );
        assert!(
            parse_stamp_args(&[
                "integration".into(),
                "relay".into(),
                "claude-code".into(),
                "PreToolUse".into()
            ])
            .is_none()
        );
    }

    #[test]
    fn stamp_bytes_merges_claude_effort_object() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".heddle")).unwrap();
        stamp_bytes(
            dir.path(),
            "claude-code",
            r#"{"session_id":"s1","effort":{"level":"high"}}"#,
        )
        .unwrap();
        stamp_bytes(
            dir.path(),
            "claude-code",
            r#"{"model":{"id":"claude-opus-4-7"}}"#,
        )
        .unwrap();
        let cursor = read_identity_cursor(dir.path());
        assert_eq!(cursor.model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(cursor.thought_level.as_deref(), Some("high"));
        assert_eq!(cursor.session.as_deref(), Some("s1"));
        let raw = fs::read_to_string(identity_cursor_path(dir.path())).unwrap();
        assert!(raw.len() < 400);
    }

    #[test]
    fn session_end_stamp_expires_cursor() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".heddle")).unwrap();
        stamp_bytes(
            dir.path(),
            "claude-code",
            r#"{"session_id":"s1","model":{"id":"claude-opus-4-7"}}"#,
        )
        .unwrap();
        assert_eq!(
            read_identity_cursor(dir.path()).model.as_deref(),
            Some("claude-opus-4-7")
        );
        stamp_bytes(
            dir.path(),
            "claude-code",
            r#"{"hook_event_name":"SessionEnd","session_id":"s1"}"#,
        )
        .unwrap();
        assert!(
            read_identity_cursor(dir.path()).is_empty(),
            "SessionEnd must expire the cursor so a later capture cannot freeze it"
        );
        assert!(!identity_cursor_path(dir.path()).exists());
    }

    #[test]
    fn parse_recognises_expire_flag() {
        let parsed = parse_stamp_args(&[
            "integration".into(),
            "stamp".into(),
            "codex".into(),
            "--expire".into(),
        ])
        .expect("codex stamp --expire is a stamp invocation");
        assert!(parsed.expire);
        assert_eq!(parsed.harness, "codex");
    }

    #[test]
    fn codex_stop_stamp_expires_cursor() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".heddle")).unwrap();
        stamp_bytes(
            dir.path(),
            "codex",
            r#"{"model":"gpt-5.4","session_id":"c1"}"#,
        )
        .unwrap();
        assert_eq!(
            read_identity_cursor(dir.path()).model.as_deref(),
            Some("gpt-5.4")
        );
        stamp_bytes(
            dir.path(),
            "codex",
            r#"{"hook_event_name":"Stop","session_id":"c1"}"#,
        )
        .unwrap();
        assert!(
            read_identity_cursor(dir.path()).is_empty(),
            "Codex Stop must expire the cursor so a later capture cannot freeze it"
        );
        assert!(!identity_cursor_path(dir.path()).exists());
    }

    #[test]
    fn expire_flag_removes_cursor_without_payload() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".heddle")).unwrap();
        stamp_bytes(
            dir.path(),
            "codex",
            r#"{"model":"gpt-5.4","session_id":"c1"}"#,
        )
        .unwrap();
        apply_identity_stamp(dir.path(), "codex", "", true).unwrap();
        assert!(read_identity_cursor(dir.path()).is_empty());
        assert!(!identity_cursor_path(dir.path()).exists());
    }

    #[test]
    fn parent_session_stamp_clears_stale_subagent_parent() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".heddle")).unwrap();
        stamp_bytes(
            dir.path(),
            "codex",
            r#"{"session_id":"sub-1","parent_id":"parent-1","model":"gpt-5.4"}"#,
        )
        .unwrap();
        assert_eq!(
            read_identity_cursor(dir.path()).parent.as_deref(),
            Some("parent-1")
        );
        stamp_bytes(
            dir.path(),
            "codex",
            r#"{"session_id":"parent-1","model":"gpt-5.4"}"#,
        )
        .unwrap();
        let cursor = read_identity_cursor(dir.path());
        assert_eq!(cursor.session.as_deref(), Some("parent-1"));
        assert!(
            cursor.parent.is_none(),
            "later parent-session stamp must not keep the subagent parent"
        );
    }
}
