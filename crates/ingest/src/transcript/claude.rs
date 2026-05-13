// SPDX-License-Identifier: Apache-2.0
//! Load a Claude Code session (JSONL) into a normalized [`Transcript`].
//!
//! # On-disk shape
//!
//! Claude Code writes one JSONL file per session under
//! `~/.claude/projects/<path-slug>/<session-uuid>.jsonl`. Each line is a
//! self-describing event. The types we care about:
//!
//! | `type`      | What it means                                        |
//! |-------------|-------------------------------------------------------|
//! | `user`      | A user turn (text or tool_result). Has `cwd`, `timestamp`, `sessionId`, `gitBranch`. |
//! | `assistant` | A model turn. Same top-level fields plus `message.content[]` — an array of `text` / `tool_use` / `thinking` blocks. |
//! | `system`    | System-side events (auto-compact, notices). Carry `timestamp` but not usually file-level signal. |
//! | `progress`, `queue-operation`, `last-prompt` | Telemetry/bookkeeping we skip. |
//!
//! We extract:
//!
//! - `sessionId` → [`Transcript::session_id`]
//! - `cwd` (first non-empty wins) → [`Transcript::cwd`]
//! - Earliest/latest `timestamp` across all events → started/ended
//! - `tool_use` blocks named `Edit` / `Write` / `Read` / `NotebookEdit` →
//!   [`FileTouch`] entries
//!
//! Other tools (Bash, Grep, Glob, Agent, MCP tools, AskUserQuestion, …)
//! are ignored: they either don't imply file edits, or the file set is
//! opaque to us (e.g. Bash could do anything, but we've already captured
//! the Edits it would normally replace).

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use tracing::debug;

use super::types::{FileTouch, Provider, TouchKind, Transcript};
use crate::IngestError;

/// Load a single session `.jsonl`, returning `Ok(None)` if the file
/// contains no events we could normalize (empty sessions happen when a
/// user hits Ctrl-C before the first turn).
pub fn load(path: impl AsRef<Path>) -> crate::Result<Option<Transcript>> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).map_err(|e| {
        IngestError::Io(std::io::Error::new(
            e.kind(),
            format!("reading claude session {}: {e}", path.display()),
        ))
    })?;
    parse(&text, path)
}

/// Internal entry point, split out so tests don't need a file on disk.
pub(super) fn parse(text: &str, source_path: &Path) -> crate::Result<Option<Transcript>> {
    let mut session_id: Option<String> = None;
    let mut cwd: Option<PathBuf> = None;
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
                // A malformed line shouldn't sink the whole session —
                // Claude occasionally flushes partial events on crash.
                debug!(
                    source = %source_path.display(),
                    line = line_no + 1,
                    error = %e,
                    "skipping malformed Claude event"
                );
                continue;
            }
        };

        // Session id and cwd come from the first event that carries them
        // (they're redundant across events but first-wins is deterministic).
        if session_id.is_none() {
            session_id = event.session_id.clone();
        }
        if cwd.is_none()
            && let Some(c) = event.cwd.as_ref()
        {
            cwd = Some(PathBuf::from(c));
        }

        if let Some(ts) = event.timestamp {
            started = Some(started.map_or(ts, |s| s.min(ts)));
            ended = Some(ended.map_or(ts, |e| e.max(ts)));
        }

        match event.event_type.as_deref() {
            Some("user") | Some("assistant") => turn_count += 1,
            _ => {}
        }

        if event.event_type.as_deref() == Some("assistant")
            && let Some(content) = event.message.as_ref().and_then(|m| m.content.as_ref())
        {
            for block in content.blocks() {
                if block.block_type.as_deref() != Some("tool_use") {
                    continue;
                }
                let Some(name) = block.name.as_deref() else {
                    continue;
                };
                let kind = match name {
                    // `Write` replaces, `Edit` modifies, `MultiEdit` does
                    // several Edits in one call, `NotebookEdit` targets a
                    // notebook cell. All count as writes.
                    "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => TouchKind::Write,
                    "Read" => TouchKind::Read,
                    _ => continue,
                };
                let Some(input) = block.input.as_ref() else {
                    continue;
                };
                // `NotebookEdit` uses `notebook_path`; the others
                // use `file_path`. Accept either.
                let path_str = input
                    .file_path
                    .as_deref()
                    .or(input.notebook_path.as_deref());
                let Some(path) = path_str else { continue };
                let Some(ts) = event.timestamp else { continue };
                touches.push(FileTouch {
                    path: PathBuf::from(path),
                    timestamp: ts,
                    kind,
                });
            }
        }
    }

    let (Some(session_id), Some(started_at), Some(ended_at)) = (session_id, started, ended) else {
        return Ok(None);
    };

    Ok(Some(Transcript {
        provider: Provider::Claude,
        session_id,
        source_path: source_path.to_path_buf(),
        cwd,
        started_at,
        ended_at,
        turn_count,
        files_touched: touches,
        starting_commit: None,
    }))
}

/// Top-level JSONL event. Fields are `Option` because different event
/// types carry different keys and we want serde to tolerate missing ones.
#[derive(Debug, Deserialize)]
struct RawEvent {
    #[serde(rename = "type")]
    event_type: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    cwd: Option<String>,
    timestamp: Option<DateTime<Utc>>,
    message: Option<RawMessage>,
}

#[derive(Debug, Deserialize)]
struct RawMessage {
    /// Claude Code's `content` field is polymorphic: assistant turns
    /// almost always carry a `Vec<RawBlock>` (typed `text`/`tool_use`/
    /// `thinking` blocks), but user turns *and* a non-trivial fraction of
    /// assistant turns flatten to a plain string. Modelling the field as
    /// an untagged enum lets us tolerate both — silently dropping either
    /// shape would lose ~half the events on a typical session.
    content: Option<RawContent>,
}

/// Untagged so serde tries `Blocks` first (richer shape) and falls back
/// to `Text`. Shorter `String` matches before `Vec<RawBlock>` would
/// succeed against an array, so this ordering is safe.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawContent {
    /// Plain-string content. We only mine `tool_use` blocks for file
    /// touches, so a string-only event contributes turn count + window
    /// timestamps and nothing else — same as a `text`-only Blocks event.
    Text(String),
    Blocks(Vec<RawBlock>),
}

impl RawContent {
    /// Yield the typed blocks if any, or an empty slice for the string
    /// case. Lets the caller iterate uniformly.
    fn blocks(&self) -> &[RawBlock] {
        match self {
            RawContent::Blocks(b) => b.as_slice(),
            RawContent::Text(_) => &[],
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawBlock {
    #[serde(rename = "type")]
    block_type: Option<String>,
    name: Option<String>,
    input: Option<RawToolInput>,
}

#[derive(Debug, Deserialize)]
struct RawToolInput {
    file_path: Option<String>,
    notebook_path: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jsonl(lines: &[&str]) -> String {
        lines.join("\n")
    }

    #[test]
    fn extracts_session_id_cwd_and_turn_window() {
        let text = jsonl(&[
            r#"{"type":"user","sessionId":"S","cwd":"/repo","timestamp":"2026-04-21T10:00:00Z","uuid":"u1"}"#,
            r#"{"type":"assistant","sessionId":"S","cwd":"/repo","timestamp":"2026-04-21T10:01:00Z","uuid":"a1","message":{"content":[{"type":"text","text":"hi"}]}}"#,
            r#"{"type":"user","sessionId":"S","cwd":"/repo","timestamp":"2026-04-21T10:02:30Z","uuid":"u2"}"#,
        ]);
        let t = parse(&text, Path::new("/sess.jsonl")).unwrap().unwrap();
        assert_eq!(t.session_id, "S");
        assert_eq!(t.cwd, Some(PathBuf::from("/repo")));
        assert_eq!(t.provider, Provider::Claude);
        assert_eq!(t.turn_count, 3);
        // 10:00 → 10:02:30
        assert_eq!(
            t.ended_at.signed_duration_since(t.started_at),
            chrono::Duration::seconds(150)
        );
    }

    #[test]
    fn picks_up_edit_write_and_read_tool_uses() {
        // NOTE: each JSONL event must live on a single line — `parse`
        // splits on `\n` before feeding serde, so embedded newlines in
        // the fixture would get parsed as separate (broken) events.
        let text = jsonl(&[
            r#"{"type":"assistant","sessionId":"S","cwd":"/repo","timestamp":"2026-04-21T10:00:00Z","uuid":"a1","message":{"content":[{"type":"tool_use","name":"Write","input":{"file_path":"/repo/a.rs","content":"x"}},{"type":"tool_use","name":"Edit","input":{"file_path":"/repo/a.rs","old_string":"x","new_string":"y"}},{"type":"tool_use","name":"Read","input":{"file_path":"/repo/b.rs"}}]}}"#,
        ]);
        let t = parse(&text, Path::new("/s.jsonl")).unwrap().unwrap();
        assert_eq!(t.files_touched.len(), 3);
        assert_eq!(t.files_touched[0].kind, TouchKind::Write);
        assert_eq!(t.files_touched[1].kind, TouchKind::Write);
        assert_eq!(t.files_touched[2].kind, TouchKind::Read);
        assert_eq!(t.files_touched[0].path, PathBuf::from("/repo/a.rs"));
    }

    #[test]
    fn ignores_non_file_tools() {
        let text = jsonl(&[
            r#"{"type":"assistant","sessionId":"S","cwd":"/r","timestamp":"2026-04-21T10:00:00Z","uuid":"a","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}},{"type":"tool_use","name":"Grep","input":{"pattern":"foo"}},{"type":"tool_use","name":"AskUserQuestion","input":{"questions":[]}}]}}"#,
        ]);
        let t = parse(&text, Path::new("/s.jsonl")).unwrap().unwrap();
        assert!(t.files_touched.is_empty(), "{:?}", t.files_touched);
    }

    #[test]
    fn malformed_line_is_skipped_not_fatal() {
        let text = jsonl(&[
            r#"{"type":"user","sessionId":"S","cwd":"/r","timestamp":"2026-04-21T10:00:00Z"}"#,
            "not valid json",
            r#"{"type":"user","sessionId":"S","cwd":"/r","timestamp":"2026-04-21T10:01:00Z"}"#,
        ]);
        let t = parse(&text, Path::new("/s.jsonl")).unwrap().unwrap();
        assert_eq!(t.turn_count, 2);
    }

    #[test]
    fn notebook_edit_uses_notebook_path() {
        let text = jsonl(&[
            r#"{"type":"assistant","sessionId":"S","cwd":"/r","timestamp":"2026-04-21T10:00:00Z","uuid":"a","message":{"content":[{"type":"tool_use","name":"NotebookEdit","input":{"notebook_path":"/r/nb.ipynb","new_source":"x"}}]}}"#,
        ]);
        let t = parse(&text, Path::new("/s.jsonl")).unwrap().unwrap();
        assert_eq!(t.files_touched.len(), 1);
        assert_eq!(t.files_touched[0].path, PathBuf::from("/r/nb.ipynb"));
    }

    #[test]
    fn user_turn_with_string_content_does_not_drop_event() {
        // Regression for the silent-drop bug: Claude Code emits user
        // turns whose `message.content` is a plain string, but the parser
        // used to type that field as `Option<Vec<RawBlock>>` and serde
        // bailed with `invalid type: string … expected a sequence`. The
        // event then got logged as `skipping malformed Claude event` and
        // its timestamp + turn count vanished. With the polymorphic
        // RawContent enum the event lands cleanly.
        let text = jsonl(&[
            r#"{"type":"user","sessionId":"S","cwd":"/r","timestamp":"2026-04-21T10:00:00Z","message":{"role":"user","content":"fix `cargo clippy --workspace`"}}"#,
            r#"{"type":"assistant","sessionId":"S","cwd":"/r","timestamp":"2026-04-21T10:01:00Z","uuid":"a1","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/r/a.rs","old_string":"x","new_string":"y"}}]}}"#,
        ]);
        let t = parse(&text, Path::new("/sess.jsonl")).unwrap().unwrap();
        // Both turns counted — the user-string event is no longer dropped.
        assert_eq!(t.turn_count, 2);
        // The assistant tool_use still produced a touch.
        assert_eq!(t.files_touched.len(), 1);
        assert_eq!(t.files_touched[0].path, PathBuf::from("/r/a.rs"));
    }

    #[test]
    fn assistant_turn_with_string_content_counts_but_emits_no_touch() {
        // Some assistant turns also flatten to a string when there's no
        // tool call (e.g. a one-line "Done."). The harvester sees an
        // empty block list — no touches, no candidates — but the turn
        // still contributes to the session's window/turn-count.
        let text = jsonl(&[
            r#"{"type":"assistant","sessionId":"S","cwd":"/r","timestamp":"2026-04-21T10:00:00Z","uuid":"a1","message":{"content":"Done."}}"#,
        ]);
        let t = parse(&text, Path::new("/sess.jsonl")).unwrap().unwrap();
        assert_eq!(t.turn_count, 1);
        assert!(t.files_touched.is_empty(), "got: {:?}", t.files_touched);
    }

    #[test]
    fn empty_session_returns_none() {
        let t = parse("", Path::new("/empty.jsonl")).unwrap();
        assert!(t.is_none());
    }

    #[test]
    fn session_without_timestamps_returns_none() {
        // A session whose every event is missing `timestamp` can't bound
        // its own window — safer to drop it than guess.
        let text = jsonl(&[
            r#"{"type":"queue-operation","sessionId":"S"}"#,
            r#"{"type":"system","sessionId":"S"}"#,
        ]);
        let t = parse(&text, Path::new("/s.jsonl")).unwrap();
        assert!(t.is_none());
    }
}