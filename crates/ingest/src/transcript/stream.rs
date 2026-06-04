// SPDX-License-Identifier: Apache-2.0
//! One shared low-level event stream per transcript format.
//!
//! Each format (`claude`, `codex`) walks its on-disk shape exactly once
//! and yields a sequence of normalized [`StreamEvent`]s. Both consumers
//! fold over the same stream:
//!
//! - the [`Transcript`] builder ([`fold_transcript`]) — extracts the
//!   session window, cwd, turn count, and file touches;
//! - the reasoning harvester ([`crate::reasoning_extract`]) — projects
//!   each event into its `AssistantEvent` shape.
//!
//! Keeping the parse / cwd-tracking / tool-gate machinery in one place
//! per format removes the carbon-copied raw-event struct families and
//! the second dispatch the two consumers used to maintain in parallel.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use super::types::{FileTouch, Provider, Transcript};
use crate::IngestError;

/// One normalized event from a transcript format walk.
///
/// A single event may carry session metadata, a cwd signal, assistant
/// prose, and/or file touches. Empty fields mean "this event doesn't
/// contribute that signal" — e.g. a Claude `system` line has only a
/// timestamp.
pub(crate) struct StreamEvent {
    /// Event time, when present. Bounds the session window. Events with
    /// no timestamp still contribute metadata to the transcript but are
    /// skipped entirely by the harvester (it has nothing to anchor).
    pub timestamp: Option<DateTime<Utc>>,
    /// First-wins session id hint.
    pub session_id: Option<String>,
    /// First-wins starting-commit hint (Codex `session_meta.git`).
    pub starting_commit: Option<String>,
    /// Working-directory signal for [`Transcript::cwd`].
    pub cwd: Option<CwdSignal>,
    /// Whether this event increments `turn_count`.
    pub is_turn: bool,
    /// Assistant narrative prose, verbatim. Populated only for assistant
    /// messages; the harvester mines these for reasoning candidates.
    pub texts: Vec<String>,
    /// File interactions this event performed, already cwd-resolved and
    /// kind-classified. The harvester discards the kind and keeps paths.
    pub touches: Vec<FileTouch>,
}

impl StreamEvent {
    /// A bare event that contributes only its timestamp to the window
    /// (Claude `system`, Codex `event_msg`, redacted reasoning, …).
    pub(crate) fn bare(timestamp: Option<DateTime<Utc>>) -> Self {
        Self {
            timestamp,
            session_id: None,
            starting_commit: None,
            cwd: None,
            is_turn: false,
            texts: Vec::new(),
            touches: Vec::new(),
        }
    }
}

/// How an event's cwd should fold into [`Transcript::cwd`].
pub(crate) enum CwdSignal {
    /// Set only if no cwd has been seen yet (Claude's per-line `cwd`,
    /// Codex's `session_meta.cwd`).
    IfUnset(PathBuf),
    /// Overwrite unconditionally (Codex's mid-session `turn_context`).
    Force(PathBuf),
}

/// Fold a format's event stream into a [`Transcript`]. Returns `None`
/// when the stream carries no usable signal (no session id, or no event
/// ever bounded the time window) — mirroring the loaders' "skip quietly"
/// behavior for empty/aborted sessions.
pub(crate) fn fold_transcript(
    provider: Provider,
    source_path: &Path,
    events: impl IntoIterator<Item = StreamEvent>,
) -> Option<Transcript> {
    let mut session_id: Option<String> = None;
    let mut cwd: Option<PathBuf> = None;
    let mut starting_commit: Option<String> = None;
    let mut started: Option<DateTime<Utc>> = None;
    let mut ended: Option<DateTime<Utc>> = None;
    let mut turn_count: u32 = 0;
    let mut touches: Vec<FileTouch> = Vec::new();

    for ev in events {
        if let Some(ts) = ev.timestamp {
            started = Some(started.map_or(ts, |s: DateTime<Utc>| s.min(ts)));
            ended = Some(ended.map_or(ts, |e: DateTime<Utc>| e.max(ts)));
        }
        if session_id.is_none() {
            session_id = ev.session_id;
        }
        if starting_commit.is_none() {
            starting_commit = ev.starting_commit;
        }
        match ev.cwd {
            Some(CwdSignal::IfUnset(c)) if cwd.is_none() => cwd = Some(c),
            Some(CwdSignal::IfUnset(_)) => {}
            Some(CwdSignal::Force(c)) => cwd = Some(c),
            None => {}
        }
        if ev.is_turn {
            turn_count += 1;
        }
        touches.extend(ev.touches);
    }

    let (Some(session_id), Some(started_at), Some(ended_at)) = (session_id, started, ended) else {
        return None;
    };

    Some(Transcript {
        provider,
        session_id,
        source_path: source_path.to_path_buf(),
        cwd,
        started_at,
        ended_at,
        turn_count,
        files_touched: touches,
        starting_commit,
    })
}

/// Read a session file into a string, wrapping IO errors with the kind
/// of source we were reading. Shared by the loaders and the harvesters
/// so the error envelope is identical everywhere.
pub(crate) fn read_session_text(path: &Path, what: &str) -> crate::Result<String> {
    std::fs::read_to_string(path).map_err(|e| {
        IngestError::Io(std::io::Error::new(
            e.kind(),
            format!("reading {what} {}: {e}", path.display()),
        ))
    })
}
