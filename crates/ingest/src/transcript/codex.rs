// SPDX-License-Identifier: Apache-2.0
//! Load a Codex rollout (JSONL) into a normalized [`Transcript`].
//!
//! # On-disk shape
//!
//! Codex Desktop writes one JSONL per session under
//! `~/.codex/sessions/<YYYY>/<MM>/<DD>/rollout-<timestamp>-<uuid>.jsonl`.
//! Structure differs from Claude's:
//!
//! | `type`           | Payload shape                                     |
//! |------------------|----------------------------------------------------|
//! | `session_meta`   | `payload.{id, timestamp, cwd, git.{commit_hash}, …}` — always the first line. |
//! | `turn_context`   | Change-of-workdir/model within a session.         |
//! | `event_msg`      | Framework telemetry (task_started, token_count, …). |
//! | `response_item`  | Actual model-side events: `message`, `function_call`, `function_call_output`, `reasoning`. |
//! | `compacted`      | Context-window compaction marker.                 |
//!
//! Codex doesn't expose a structured `Edit`/`Write` tool — file mutations
//! happen inside shell `function_call`s (almost always
//! `name == "exec_command"`, arguments JSON `{"cmd": "<shell>", …}`).
//! We extract file paths by parsing the shell string for:
//!
//! - `apply_patch` heredoc blocks with `*** Update|Add|Delete File: <path>`
//! - Redirects: `>`, `>>`, `| tee`, `| tee -a`
//! - Simple readers: `cat <path>`, `rg … <path>`, `head/tail … <path>`
//!
//! This is a heuristic. It won't catch every edit (e.g. `sed -i` on a
//! path buried mid-command). The matcher handles misses gracefully — cwd
//! and time window still narrow the candidate set.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use tracing::debug;

use super::types::{FileTouch, Provider, TouchKind, Transcript};
use crate::IngestError;

pub fn load(path: impl AsRef<Path>) -> crate::Result<Option<Transcript>> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).map_err(|e| {
        IngestError::Io(std::io::Error::new(
            e.kind(),
            format!("reading codex session {}: {e}", path.display()),
        ))
    })?;
    parse(&text, path)
}

pub(super) fn parse(text: &str, source_path: &Path) -> crate::Result<Option<Transcript>> {
    let mut session_id: Option<String> = None;
    let mut cwd: Option<PathBuf> = None;
    let mut starting_commit: Option<String> = None;
    let mut started: Option<DateTime<Utc>> = None;
    let mut ended: Option<DateTime<Utc>> = None;
    let mut turn_count: u32 = 0;
    let mut touches: Vec<FileTouch> = Vec::new();

    for (line_no, raw) in text.lines().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        let event: RawEvent = match serde_json::from_str(raw) {
            Ok(e) => e,
            Err(e) => {
                debug!(
                    source = %source_path.display(),
                    line = line_no + 1,
                    error = %e,
                    "skipping malformed Codex event"
                );
                continue;
            }
        };

        if let Some(ts) = event.timestamp {
            started = Some(started.map_or(ts, |s| s.min(ts)));
            ended = Some(ended.map_or(ts, |e| e.max(ts)));
        }

        match event.event_type.as_deref() {
            Some("session_meta") => {
                if let Some(p) = event.payload.as_ref() {
                    if session_id.is_none() {
                        session_id = p.get("id").and_then(Value::as_str).map(String::from);
                    }
                    if cwd.is_none() {
                        cwd = p.get("cwd").and_then(Value::as_str).map(PathBuf::from);
                    }
                    if starting_commit.is_none() {
                        starting_commit = p
                            .get("git")
                            .and_then(|g| g.get("commit_hash"))
                            .and_then(Value::as_str)
                            .map(String::from);
                    }
                }
            }
            Some("turn_context") => {
                // A workdir switch mid-session: respect the newest cwd
                // without clobbering the session_meta cwd if no switch is
                // actually logged.
                if let Some(p) = event.payload.as_ref()
                    && let Some(new_cwd) = p.get("cwd").and_then(Value::as_str)
                {
                    cwd = Some(PathBuf::from(new_cwd));
                }
            }
            Some("response_item") => {
                let Some(p) = event.payload.as_ref() else {
                    continue;
                };
                let payload_type = p.get("type").and_then(Value::as_str).unwrap_or("");
                match payload_type {
                    "message" => {
                        turn_count += 1;
                    }
                    "function_call" => {
                        turn_count += 1;
                        let name = p.get("name").and_then(Value::as_str).unwrap_or("");
                        if name != "exec_command" {
                            continue;
                        }
                        let Some(args_str) = p.get("arguments").and_then(Value::as_str) else {
                            continue;
                        };
                        let Ok(args_json) = serde_json::from_str::<Value>(args_str) else {
                            continue;
                        };
                        let Some(cmd) = args_json.get("cmd").and_then(Value::as_str) else {
                            continue;
                        };
                        let ts = match event.timestamp {
                            Some(ts) => ts,
                            None => continue,
                        };
                        let base_cwd = args_json
                            .get("workdir")
                            .and_then(Value::as_str)
                            .map(PathBuf::from)
                            .or_else(|| cwd.clone());
                        extract_shell_touches(cmd, ts, base_cwd.as_deref(), &mut touches);
                    }
                    "custom_tool_call" => {
                        let name = p.get("name").and_then(Value::as_str).unwrap_or("");
                        if name != "apply_patch" {
                            continue;
                        }
                        let Some(input) = p.get("input").and_then(Value::as_str) else {
                            continue;
                        };
                        let ts = match event.timestamp {
                            Some(ts) => ts,
                            None => continue,
                        };
                        extract_shell_touches(input, ts, cwd.as_deref(), &mut touches);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let (Some(session_id), Some(started_at), Some(ended_at)) = (session_id, started, ended) else {
        return Ok(None);
    };

    Ok(Some(Transcript {
        provider: Provider::Codex,
        session_id,
        source_path: source_path.to_path_buf(),
        cwd,
        started_at,
        ended_at,
        turn_count,
        files_touched: touches,
        starting_commit,
    }))
}

/// Parse a single `exec_command` shell string for file references. Results
/// are appended to `out`. This is deliberately conservative — false
/// negatives (missed touches) are fine, false positives (noise) pollute
/// the matcher.
///
/// Exposed `pub(crate)` so the reasoning harvester can reuse the exact
/// same shell-parsing rules the matcher's loader uses, instead of
/// re-deriving them and risking drift.
pub(crate) fn extract_shell_touches(
    cmd: &str,
    ts: DateTime<Utc>,
    base_cwd: Option<&Path>,
    out: &mut Vec<FileTouch>,
) {
    // 1. `apply_patch` heredoc blocks.
    //
    // Codex's apply_patch uses a custom diff grammar with sentinel lines:
    //   *** Update File: path/to/foo.rs
    //   *** Add File: path/to/new.rs
    //   *** Delete File: path/to/old.rs
    //
    // We scan for those prefixes line-by-line. The path is the rest of
    // the line, trimmed. No need to consume the body.
    for line in cmd.lines() {
        let trimmed = line.trim_start();
        let (kind, rest) = if let Some(r) = trimmed.strip_prefix("*** Update File:") {
            (TouchKind::Write, r)
        } else if let Some(r) = trimmed.strip_prefix("*** Add File:") {
            (TouchKind::Write, r)
        } else if let Some(r) = trimmed.strip_prefix("*** Delete File:") {
            (TouchKind::Delete, r)
        } else {
            continue;
        };
        let path = rest.trim();
        if path.is_empty() {
            continue;
        }
        out.push(FileTouch {
            path: resolve_path(path, base_cwd),
            timestamp: ts,
            kind,
        });
    }

    // 2. Redirect writes. We match `> path` and `>> path` but not `2>` /
    // `&>` / `>/dev/null` (the last one is noise). Tokenization is
    // whitespace-based, which is enough for the simple commands Codex
    // emits.
    //
    // This is independent of the heredoc scan above; a single command
    // could both apply_patch *and* redirect.
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i];
        let path_opt: Option<&str> = if tok == ">" || tok == ">>" {
            tokens.get(i + 1).copied()
        } else if let Some(rest) = tok.strip_prefix(">>") {
            // `>>foo` form
            (!rest.is_empty()).then_some(rest)
        } else if let Some(rest) = tok
            .strip_prefix('>')
            .filter(|r| !r.starts_with('>') && !r.starts_with('&'))
        {
            (!rest.is_empty()).then_some(rest)
        } else {
            None
        };
        if let Some(path) = path_opt {
            if looks_like_file_path(path) {
                out.push(FileTouch {
                    path: resolve_path(path, base_cwd),
                    timestamp: ts,
                    kind: TouchKind::Write,
                });
            }
            i += if tok == ">" || tok == ">>" { 2 } else { 1 };
            continue;
        }
        i += 1;
    }

    // 3. Obvious readers. These are weaker signals but help when a
    // session is otherwise quiet (e.g. a debug-only session that reads a
    // lot and commits a one-liner).
    //
    // We only look at the first token of the (sub)command. Pipelines
    // like `cat foo | head` still get `cat foo`.
    if let Some(first) = tokens.first() {
        let reader = matches!(*first, "cat" | "head" | "tail" | "less" | "wc" | "file");
        if reader {
            for arg in tokens.iter().skip(1) {
                if looks_like_file_path(arg) {
                    out.push(FileTouch {
                        path: resolve_path(arg, base_cwd),
                        timestamp: ts,
                        kind: TouchKind::Read,
                    });
                }
            }
        }
    }
}

/// `true` if the token looks like a path worth recording. We filter out
/// flags (`-foo`), shell specials (`/dev/null`, `&1`, `&2`), empty
/// strings, and pure numbers.
fn looks_like_file_path(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    if token.starts_with('-') {
        return false;
    }
    if token.starts_with('&') {
        return false;
    }
    if token == "/dev/null" || token.starts_with("/dev/") {
        return false;
    }
    // Must contain a path-ish char (`/` or `.`) to avoid flagging
    // positional args like `origin main` in `git push origin main`.
    if !token.contains('/') && !token.contains('.') {
        return false;
    }
    // Strip surrounding quotes if any — shell tokenization is
    // whitespace-only so quoted paths keep their quotes attached.
    let stripped = token.trim_matches(|c| c == '\'' || c == '"');
    !stripped.is_empty()
}

fn resolve_path(raw: &str, base: Option<&Path>) -> PathBuf {
    // Strip quotes the whitespace tokenizer leaves behind.
    let s = raw.trim_matches(|c| c == '\'' || c == '"');
    let p = PathBuf::from(s);
    if p.is_absolute() {
        return p;
    }
    match base {
        Some(b) => b.join(p),
        None => p,
    }
}

#[derive(Debug, Deserialize)]
struct RawEvent {
    #[serde(rename = "type")]
    event_type: Option<String>,
    timestamp: Option<DateTime<Utc>>,
    payload: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jsonl(lines: &[&str]) -> String {
        lines.join("\n")
    }

    #[test]
    fn session_meta_drives_id_cwd_and_starting_commit() {
        let text = jsonl(&[
            r#"{"timestamp":"2026-04-21T10:00:00Z","type":"session_meta","payload":{"id":"SID","cwd":"/repo","git":{"commit_hash":"deadbeef"}}}"#,
            r#"{"timestamp":"2026-04-21T10:01:00Z","type":"event_msg","payload":{}}"#,
        ]);
        let t = parse(&text, Path::new("/s.jsonl")).unwrap().unwrap();
        assert_eq!(t.session_id, "SID");
        assert_eq!(t.cwd, Some(PathBuf::from("/repo")));
        assert_eq!(t.starting_commit, Some("deadbeef".into()));
        assert_eq!(t.provider, Provider::Codex);
    }

    #[test]
    fn exec_command_apply_patch_is_extracted() {
        // Codex's apply_patch wrapped in a shell heredoc. The important
        // bit is the `*** Update|Add|Delete File:` sentinel lines.
        let cmd = "apply_patch <<'PATCH'\n\
*** Begin Patch\n\
*** Update File: crates/foo.rs\n\
@@\n\
-old\n\
+new\n\
*** Add File: crates/bar.rs\n\
+x\n\
*** Delete File: crates/gone.rs\n\
*** End Patch\n\
PATCH";
        let args = serde_json::json!({ "cmd": cmd, "workdir": "/repo" }).to_string();
        let text = jsonl(&[
            r#"{"timestamp":"2026-04-21T10:00:00Z","type":"session_meta","payload":{"id":"S","cwd":"/repo"}}"#,
            &format!(
                r#"{{"timestamp":"2026-04-21T10:01:00Z","type":"response_item","payload":{{"type":"function_call","name":"exec_command","arguments":{}}}}}"#,
                serde_json::to_string(&args).unwrap()
            ),
        ]);
        let t = parse(&text, Path::new("/s.jsonl")).unwrap().unwrap();
        let paths: Vec<_> = t
            .files_touched
            .iter()
            .map(|f| (f.path.clone(), f.kind))
            .collect();
        assert_eq!(
            paths,
            vec![
                (PathBuf::from("/repo/crates/foo.rs"), TouchKind::Write),
                (PathBuf::from("/repo/crates/bar.rs"), TouchKind::Write),
                (PathBuf::from("/repo/crates/gone.rs"), TouchKind::Delete),
            ],
            "got {:?}",
            t.files_touched
        );
    }

    #[test]
    fn redirect_writes_are_caught_and_devnull_is_ignored() {
        let cmd = "echo hello > src/out.txt 2>/dev/null && ls >>logs/run.log";
        let args = serde_json::json!({"cmd": cmd, "workdir": "/repo"}).to_string();
        let text = jsonl(&[
            r#"{"timestamp":"2026-04-21T10:00:00Z","type":"session_meta","payload":{"id":"S","cwd":"/repo"}}"#,
            &format!(
                r#"{{"timestamp":"2026-04-21T10:01:00Z","type":"response_item","payload":{{"type":"function_call","name":"exec_command","arguments":{}}}}}"#,
                serde_json::to_string(&args).unwrap()
            ),
        ]);
        let t = parse(&text, Path::new("/s.jsonl")).unwrap().unwrap();
        let paths: Vec<_> = t.files_touched.iter().map(|f| f.path.clone()).collect();
        assert!(paths.contains(&PathBuf::from("/repo/src/out.txt")));
        assert!(paths.contains(&PathBuf::from("/repo/logs/run.log")));
        assert!(
            !paths.iter().any(|p| p.to_str() == Some("/dev/null")),
            "captured /dev/null: {paths:?}"
        );
    }

    #[test]
    fn reader_commands_become_read_touches() {
        let cmd = "cat src/lib.rs README.md";
        let args = serde_json::json!({"cmd": cmd, "workdir": "/repo"}).to_string();
        let text = jsonl(&[
            r#"{"timestamp":"2026-04-21T10:00:00Z","type":"session_meta","payload":{"id":"S","cwd":"/repo"}}"#,
            &format!(
                r#"{{"timestamp":"2026-04-21T10:01:00Z","type":"response_item","payload":{{"type":"function_call","name":"exec_command","arguments":{}}}}}"#,
                serde_json::to_string(&args).unwrap()
            ),
        ]);
        let t = parse(&text, Path::new("/s.jsonl")).unwrap().unwrap();
        assert!(
            t.files_touched.iter().all(|f| f.kind == TouchKind::Read),
            "{:?}",
            t.files_touched
        );
        assert_eq!(t.files_touched.len(), 2);
    }

    #[test]
    fn non_exec_function_calls_are_ignored() {
        let args = r#"{"session_id":1,"chars":""}"#;
        let text = jsonl(&[
            r#"{"timestamp":"2026-04-21T10:00:00Z","type":"session_meta","payload":{"id":"S","cwd":"/r"}}"#,
            &format!(
                r#"{{"timestamp":"2026-04-21T10:01:00Z","type":"response_item","payload":{{"type":"function_call","name":"write_stdin","arguments":{}}}}}"#,
                serde_json::to_string(&args).unwrap()
            ),
        ]);
        let t = parse(&text, Path::new("/s.jsonl")).unwrap().unwrap();
        assert!(t.files_touched.is_empty());
        // But the turn still counts.
        assert_eq!(t.turn_count, 1);
    }

    #[test]
    fn custom_apply_patch_tool_call_is_extracted() {
        let input = "*** Begin Patch\n\
*** Update File: src/native.rs\n\
@@\n\
-old\n\
+new\n\
*** End Patch\n";
        let text = jsonl(&[
            r#"{"timestamp":"2026-04-21T10:00:00Z","type":"session_meta","payload":{"id":"S","cwd":"/repo"}}"#,
            &format!(
                r#"{{"timestamp":"2026-04-21T10:01:00Z","type":"response_item","payload":{{"type":"custom_tool_call","status":"completed","name":"apply_patch","input":{}}}}}"#,
                serde_json::to_string(input).unwrap()
            ),
        ]);
        let t = parse(&text, Path::new("/s.jsonl")).unwrap().unwrap();
        assert_eq!(
            t.files_touched.first().map(|f| (&f.path, f.kind)),
            Some((&PathBuf::from("/repo/src/native.rs"), TouchKind::Write))
        );
    }

    #[test]
    fn turn_context_updates_cwd_for_later_commands() {
        // If Codex switches workdir mid-session (heddle worktree jumps, say),
        // later commands should resolve relative paths against the new cwd.
        let cmd = "echo x > out.txt";
        let args = serde_json::json!({"cmd": cmd}).to_string(); // no workdir in args
        let text = jsonl(&[
            r#"{"timestamp":"2026-04-21T10:00:00Z","type":"session_meta","payload":{"id":"S","cwd":"/old"}}"#,
            r#"{"timestamp":"2026-04-21T10:00:10Z","type":"turn_context","payload":{"cwd":"/new"}}"#,
            &format!(
                r#"{{"timestamp":"2026-04-21T10:01:00Z","type":"response_item","payload":{{"type":"function_call","name":"exec_command","arguments":{}}}}}"#,
                serde_json::to_string(&args).unwrap()
            ),
        ]);
        let t = parse(&text, Path::new("/s.jsonl")).unwrap().unwrap();
        assert_eq!(t.cwd, Some(PathBuf::from("/new")));
        assert_eq!(
            t.files_touched.first().map(|f| f.path.clone()),
            Some(PathBuf::from("/new/out.txt"))
        );
    }

    #[test]
    fn flag_like_tokens_are_not_mistaken_for_paths() {
        assert!(!looks_like_file_path("-v"));
        assert!(!looks_like_file_path("--help"));
        assert!(!looks_like_file_path("main"));
        assert!(!looks_like_file_path(""));
        assert!(!looks_like_file_path("&1"));
        assert!(!looks_like_file_path("/dev/null"));
        assert!(looks_like_file_path("src/foo.rs"));
        assert!(looks_like_file_path("README.md"));
        assert!(looks_like_file_path("/abs/path/x"));
    }
}