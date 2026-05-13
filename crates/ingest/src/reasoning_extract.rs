// SPDX-License-Identifier: Apache-2.0
//! Two-stage ReasoningPoint extractor.
//!
//! Given a [`Transcript`](crate::transcript::Transcript) and the commit it
//! matches, produce zero or more [`ReasoningPoint`] records suitable for
//! attaching as annotations.
//!
//! # The two stages
//!
//! **Stage 1 — harvest** ([`harvest`]). A deterministic rule-based scan
//! that re-opens the transcript's source `.jsonl`, walks assistant
//! narrative blocks (for Claude: `text` blocks; redacted `thinking`
//! blocks carry no usable content and are skipped), splits them into
//! sentences, and yields every sentence that looks load-bearing:
//! statements containing reasoning keywords (`because`, `never`,
//! `always`, `must`, `gotcha`, `note that`, …). Each candidate is
//! tagged with a `kind_hint`, a target guess (a file path touched by a
//! tool_use block close to the sentence), and the originating turn
//! index.
//!
//! This stage is pure and cheap. It can run on every matched transcript
//! at import time with no network or LLM dependency.
//!
//! **Stage 2 — keep** ([`keep`]). A second deterministic filter that
//! takes a candidate + its evidence envelope and either:
//!
//! - promotes it to a [`ReasoningPoint`] (trimming to ≤140 chars,
//!   computing a confidence score, sealing in evidence), or
//! - drops it (too long, too short, no target, below threshold).
//!
//! The keep stage is intentionally rules-based so the importer doesn't
//! need an LLM. A future [`LlmRefiner`] trait is sketched below as an
//! explicit seam for a later LLM rewrite-and-score pass.
//!
//! # Scope
//!
//! Claude is supported end-to-end. Codex support is scaffolded (same
//! candidate pipeline) but the message reader for Codex rollouts is a
//! stub — Codex rollouts emit reasoning as prose paragraphs without the
//! Claude block taxonomy, and the signal is noisier. Landing Codex
//! properly is follow-up work; the [`harvest_from_codex_events`]
//! function is a placeholder so the shape is clear.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use objects::object::AnnotationKind;
use serde::Deserialize;
use tracing::debug;

use crate::{
    IngestError,
    reasoning::{ReasoningEvidence, ReasoningPoint, ReasoningTarget},
    transcript::{Provider, Transcript},
};

// ---------- Public types ----------

/// What a harvest step emits. Contains enough provenance for stage 2
/// to seal a [`ReasoningPoint`] without re-opening the transcript.
#[derive(Clone, Debug, PartialEq)]
pub struct HarvestedCandidate {
    /// The block-level narrative this sentence came from. Useful for
    /// debugging false positives — lets us point at the full turn.
    pub turn_index: u32,
    /// The sentence itself, verbatim. Stage 2 is responsible for
    /// trimming to the 140-char budget.
    pub text: String,
    /// Classifier hint based on which keyword fired. Stage 2 may
    /// override this after inspecting the sentence in full.
    pub kind_hint: AnnotationKind,
    /// Best-guess target: a file path touched near this sentence.
    /// `None` if no file was touched within the attention window.
    pub target_hint: Option<ReasoningTarget>,
    /// Tightness of the target-to-sentence association, 0..=1. Used
    /// to weight the final [`ReasoningPoint::confidence`].
    pub target_proximity: f32,
    /// The keyword that fired, for observability.
    pub trigger: &'static str,
    /// Originating timestamp — passes through to the evidence envelope
    /// if stage 2 keeps the candidate.
    pub when: DateTime<Utc>,
}

/// Knobs for [`harvest`].
#[derive(Clone, Debug)]
pub struct HarvestParams {
    /// Drop sentences shorter than this — keyword-only noise like
    /// "because." adds nothing.
    pub min_sentence_chars: usize,
    /// Hard cap on candidate length before stage 2 sees it. Sentences
    /// longer than this get skipped — they are almost always poorly
    /// split or are code-block prose we don't want to annotate with.
    pub max_sentence_chars: usize,
    /// Number of assistant events to look ahead for file targets once
    /// a reasoning-bearing sentence is found. Claude's pattern is
    /// usually "text block → immediately call Edit/Write" so a small
    /// window catches the typical case.
    pub target_lookahead: usize,
}

impl Default for HarvestParams {
    fn default() -> Self {
        Self {
            min_sentence_chars: 18,
            // Bumped alongside `KeepParams::max_chars` so we keep
            // candidates that fit the wider notecard budget. The harvester
            // floor is the candidate cap, the keep floor is the trim cap.
            max_sentence_chars: 480,
            // Cross-event lookahead is the main source of file-leak
            // misattribution: a text block in event-i talking about
            // component X gets paired with event-i+1's tool_use on
            // unrelated file Y. A lookahead of 1 keeps the typical
            // "explain → next-turn edit" pattern while removing the
            // long-tail cross-event leaks. Same-event tool_use (gap=0)
            // remains the primary signal; sentence-level path scanning
            // (see [`harvest_claude`]) is preferred over both.
            target_lookahead: 1,
        }
    }
}

/// Knobs for [`keep`].
#[derive(Clone, Debug)]
pub struct KeepParams {
    /// Maximum ReasoningPoint text length. Matches the 140-char note-
    /// card budget the schema enforces.
    pub max_chars: usize,
    /// Minimum confidence to retain. Below this we drop the point —
    /// better to skip than pollute the annotation graph.
    pub keep_threshold: f32,
    /// Multiplier applied to candidates with no target. File-scope
    /// annotations are still useful for ARCHITECTURE-style rules, so
    /// we don't zero them out outright.
    pub no_target_penalty: f32,
}

impl Default for KeepParams {
    fn default() -> Self {
        Self {
            // Notecards are summary-shaped, but a 140-char cap routinely
            // chopped mid-clause ("we can just rename it in place so the
            // worktree stays" — and then nothing). The actual schema has
            // no enforced cap; 280 chars is roughly twice the previous
            // budget and lets a typical reasoning sentence land whole.
            // Trimming still happens for runaway sentences, but the
            // word-boundary fallback now has room to breathe.
            max_chars: 280,
            keep_threshold: 0.55,
            no_target_penalty: 0.6,
        }
    }
}

/// Future seam: an LLM-backed stage that rewrites candidates into
/// notecard form and rescores them. Not used in the default pipeline —
/// the importer runs the deterministic pipeline only, and the caller
/// can opt in later.
pub trait LlmRefiner {
    fn refine(&self, cand: &HarvestedCandidate) -> Option<(String, f32)>;
}

// ---------- Keyword taxonomy ----------

/// One keyword rule. Ordered list; first match wins so more specific
/// phrases should come first.
struct Trigger {
    /// Lowercase substring to scan for. Must be a word-ish boundary so
    /// `"always"` doesn't fire on `"alwaysBlock"`; the scanner adds
    /// boundary checks.
    needle: &'static str,
    kind: AnnotationKind,
    /// Strength of this trigger, 0..=1. Contributes to confidence.
    strength: f32,
}

const TRIGGERS: &[Trigger] = &[
    // Constraint-flavored — sharp, non-obvious warnings.
    Trigger {
        needle: "gotcha",
        kind: AnnotationKind::Constraint,
        strength: 0.95,
    },
    Trigger {
        needle: "looks like it should",
        kind: AnnotationKind::Constraint,
        strength: 0.85,
    },
    Trigger {
        needle: "but actually ",
        kind: AnnotationKind::Constraint,
        strength: 0.7,
    },
    Trigger {
        needle: "subtle",
        kind: AnnotationKind::Constraint,
        strength: 0.6,
    },
    Trigger {
        needle: "easy to miss",
        kind: AnnotationKind::Constraint,
        strength: 0.85,
    },
    // Invariant-flavored — prescriptive rules the code must obey.
    Trigger {
        needle: "never ",
        kind: AnnotationKind::Invariant,
        strength: 0.8,
    },
    Trigger {
        needle: "always ",
        kind: AnnotationKind::Invariant,
        strength: 0.75,
    },
    Trigger {
        needle: "must not ",
        kind: AnnotationKind::Invariant,
        strength: 0.85,
    },
    Trigger {
        needle: "don't ",
        kind: AnnotationKind::Invariant,
        strength: 0.7,
    },
    Trigger {
        needle: "avoid ",
        kind: AnnotationKind::Invariant,
        strength: 0.65,
    },
    // Rationale-flavored — reasons backing a choice. Lower strength because
    // "because" fires on a lot of narration; the classifier relies on
    // target resolution to lift confidence.
    Trigger {
        needle: " because ",
        kind: AnnotationKind::Rationale,
        strength: 0.55,
    },
    Trigger {
        needle: "this is why ",
        kind: AnnotationKind::Rationale,
        strength: 0.9,
    },
    Trigger {
        needle: "the reason ",
        kind: AnnotationKind::Rationale,
        strength: 0.75,
    },
    Trigger {
        needle: "note that ",
        kind: AnnotationKind::Rationale,
        strength: 0.65,
    },
];

/// Does `s` contain `needle` at a word-ish boundary? We consider any
/// non-alphanumeric character a boundary (start and end of string
/// count too). If the needle itself begins or ends with a non-
/// alphanumeric character (like `" because "`), that edge needs no
/// external boundary check — the needle enforces it.
fn contains_keyword(s: &str, needle: &str) -> bool {
    let Some(pos) = s.find(needle) else {
        return false;
    };
    let needle_starts_alnum = needle.chars().next().is_some_and(|c| c.is_alphanumeric());
    let needle_ends_alnum = needle
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric());

    if needle_starts_alnum
        && pos > 0
        && s[..pos]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric())
    {
        return false;
    }
    if needle_ends_alnum {
        let tail_start = pos + needle.len();
        if tail_start < s.len()
            && s[tail_start..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric())
        {
            return false;
        }
    }
    true
}

fn classify(sentence_lower: &str) -> Option<(&'static Trigger, &'static str)> {
    for t in TRIGGERS {
        if contains_keyword(sentence_lower, t.needle) {
            return Some((t, t.needle));
        }
    }
    None
}

// ---------- Sentence splitting ----------

/// Split a paragraph into sentences. Deliberately dumb: splits on
/// `. ! ?` followed by whitespace, trims, ignores splits inside
/// backtick-fenced spans so "use `a.b.c`." doesn't shatter.
///
/// Returns `(sentence_text, start_offset_in_input)` tuples. The offset
/// is handy for ranking proximity when multiple sentences in one block
/// fire.
fn split_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_code = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '`' {
            in_code = !in_code;
            cur.push(c);
            continue;
        }
        cur.push(c);
        if in_code {
            continue;
        }
        if matches!(c, '.' | '!' | '?')
            && matches!(chars.peek(), Some(next) if next.is_whitespace() || *next == '\n')
        {
            push_sentence(&mut out, &mut cur);
        } else if c == '\n' && matches!(chars.peek(), Some('\n')) {
            // Blank line — a safe sentence boundary regardless of
            // punctuation. Protects us from headers and bullet lists.
            push_sentence(&mut out, &mut cur);
        }
    }
    push_sentence(&mut out, &mut cur);
    out
}

fn push_sentence(out: &mut Vec<String>, buf: &mut String) {
    let trimmed = buf.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_string());
    }
    buf.clear();
}

// ---------- Harvest: read blocks + pattern-match ----------

/// Drive stage 1 against a transcript file. Opens [`Transcript::source_path`]
/// and scans it end-to-end. Returns an empty vec (not `Err`) if the
/// transcript is missing blocks we recognize — that mirrors the "skip
/// quietly" behavior of the [`crate::transcript::claude`] loader.
pub fn harvest(t: &Transcript, params: &HarvestParams) -> crate::Result<Vec<HarvestedCandidate>> {
    match t.provider {
        Provider::Claude => harvest_claude(&t.source_path, params),
        Provider::Codex => harvest_codex(&t.source_path, params),
        // OpenCode reads from a shared SQLite file; `Transcript::source_path`
        // for an OpenCode session points at the DB itself (not a per-session
        // file the way Claude does), and the session id is enough to fetch
        // every part for that conversation.
        Provider::OpenCode => harvest_opencode(&t.source_path, &t.session_id, params),
    }
}

fn harvest_claude(path: &Path, params: &HarvestParams) -> crate::Result<Vec<HarvestedCandidate>> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        IngestError::Io(std::io::Error::new(
            e.kind(),
            format!("reading claude session {}: {e}", path.display()),
        ))
    })?;
    // Two passes: first collect the ordered assistant events, then scan
    // them with a small look-ahead queue for target resolution.
    let mut events: Vec<AssistantEvent> = Vec::new();
    for raw in text.lines() {
        if raw.trim().is_empty() {
            continue;
        }
        let Ok(e) = serde_json::from_str::<ClaudeRawEvent>(raw) else {
            continue;
        };
        if e.event_type.as_deref() != Some("assistant") {
            continue;
        }
        let Some(msg) = e.message else {
            continue;
        };
        let Some(content) = msg.content else {
            continue;
        };
        let Some(ts) = e.timestamp else { continue };

        let mut texts: Vec<String> = Vec::new();
        let mut files: Vec<PathBuf> = Vec::new();
        for b in content.blocks() {
            match b.block_type.as_deref() {
                Some("text") => {
                    if let Some(s) = b.text.as_deref()
                        && !s.trim().is_empty()
                    {
                        texts.push(s.to_string());
                    }
                }
                Some("tool_use") => {
                    if let Some(input) = b.input.as_ref() {
                        let path_str = input
                            .file_path
                            .as_deref()
                            .or(input.notebook_path.as_deref());
                        if let Some(p) = path_str {
                            files.push(PathBuf::from(p));
                        }
                    }
                }
                // `thinking` blocks are redacted at write-time; the
                // `thinking` string is empty. Nothing to harvest.
                _ => {}
            }
        }
        if !texts.is_empty() || !files.is_empty() {
            events.push(AssistantEvent {
                timestamp: ts,
                texts,
                files,
            });
        }
    }

    let out = harvest_from_events(&events, params);
    debug!(path = %path.display(), candidates = out.len(), "claude harvest");
    Ok(out)
}

/// Provider-agnostic stage 1 once events are normalized. Both Claude and
/// OpenCode build the same `AssistantEvent` shape, so the actual scoring
/// logic (sentence split, classify, target resolution) lives here exactly
/// once. Events are expected to be in chronological order; ties at the
/// same timestamp resolve by stable iteration order (caller's choice).
fn harvest_from_events(
    events: &[AssistantEvent],
    params: &HarvestParams,
) -> Vec<HarvestedCandidate> {
    // Pool of every file this session ever touched, keyed by basename.
    // When a sentence mentions a basename anywhere in its prose, that's
    // the strongest possible signal for *this sentence's* target —
    // much stronger than "what tool fired in the same event", which
    // can pair UI prose with a backend-auth edit when both happened
    // in one turn. We keep the full path keyed by basename so the
    // eventual `ReasoningTarget.file` is still the full path.
    let mut session_files_by_basename: std::collections::HashMap<String, PathBuf> =
        std::collections::HashMap::new();
    for evt in events {
        for f in &evt.files {
            if let Some(name) = f.file_name().and_then(|n| n.to_str())
                && name.chars().any(|c| c == '.' || c == '/')
                && name.len() >= 4
            {
                session_files_by_basename
                    .entry(name.to_string())
                    .or_insert_with(|| f.clone());
            }
        }
    }

    let mut out = Vec::new();
    for (idx, evt) in events.iter().enumerate() {
        let targets = nearby_targets(events, idx, params.target_lookahead);
        for paragraph in &evt.texts {
            for sentence in split_sentences(paragraph) {
                let lower = sentence.to_ascii_lowercase();
                let chars = sentence.chars().count();
                if chars < params.min_sentence_chars || chars > params.max_sentence_chars {
                    continue;
                }
                let Some((trigger, fired)) = classify(&lower) else {
                    continue;
                };
                // First preference: a session-touched basename mentioned
                // verbatim in this sentence. Falls back to the closest
                // tool_use in the lookahead window when the prose
                // doesn't name a file directly.
                let in_text = path_mentioned_in_sentence(&sentence, &session_files_by_basename);
                let (target_hint, target_proximity) = match in_text {
                    Some(p) => (
                        Some(ReasoningTarget {
                            file: p.to_string_lossy().into_owned(),
                            symbol: None,
                            line_range: None,
                        }),
                        1.0,
                    ),
                    None => pick_target(&targets),
                };
                out.push(HarvestedCandidate {
                    turn_index: idx as u32,
                    text: sentence,
                    kind_hint: trigger.kind,
                    target_hint,
                    target_proximity,
                    trigger: fired,
                    when: evt.timestamp,
                });
            }
        }
    }
    out
}

/// Return a session-touched file whose basename appears literally in the
/// sentence text. Prefers backtick-quoted mentions (most specific), then
/// any whole-word substring match. Returns the longest matching basename
/// when several candidates appear — `auth_handler.rs` wins over `auth.rs`
/// when both are mentioned, since the longer name is less ambiguous.
fn path_mentioned_in_sentence(
    sentence: &str,
    files_by_basename: &std::collections::HashMap<String, PathBuf>,
) -> Option<PathBuf> {
    if files_by_basename.is_empty() {
        return None;
    }
    let mut best: Option<(usize, &PathBuf)> = None;
    for (basename, path) in files_by_basename {
        if !sentence.contains(basename.as_str()) {
            continue;
        }
        // Reject basename-as-substring-of-a-word matches by checking the
        // boundary chars on both sides. `auth.rs` should match
        // "patched auth.rs" but not "patched_auth.rs.bak".
        if let Some(pos) = sentence.find(basename.as_str()) {
            let before = sentence[..pos].chars().next_back();
            let after = sentence[pos + basename.len()..].chars().next();
            let before_ok = before.is_none_or(|c| !c.is_alphanumeric() && c != '_');
            let after_ok = after.is_none_or(|c| !c.is_alphanumeric() && c != '_');
            if !before_ok || !after_ok {
                continue;
            }
        }
        let len = basename.len();
        if best.is_none_or(|(b, _)| len > b) {
            best = Some((len, path));
        }
    }
    best.map(|(_, p)| p.clone())
}

/// Collect a short look-ahead of file targets from the next few
/// assistant events. Each entry records the gap (in events) between
/// the text block and the edit — zero gap is the strongest signal.
fn nearby_targets(
    events: &[AssistantEvent],
    from: usize,
    lookahead: usize,
) -> VecDeque<(usize, PathBuf)> {
    let mut q = VecDeque::new();
    let end = (from + 1 + lookahead).min(events.len());
    for (j, evt) in events.iter().enumerate().take(end).skip(from) {
        let gap = j - from;
        for f in &evt.files {
            q.push_back((gap, f.clone()));
        }
    }
    q
}

/// Pick the tightest target from the look-ahead queue. Prefers the
/// edit in the same event (gap=0); falls off linearly.
fn pick_target(q: &VecDeque<(usize, PathBuf)>) -> (Option<ReasoningTarget>, f32) {
    let Some((gap, p)) = q.front() else {
        return (None, 0.0);
    };
    let target = ReasoningTarget {
        file: p.to_string_lossy().into_owned(),
        symbol: None,
        line_range: None,
    };
    // gap 0 → 1.0, gap 1 → 0.7, gap 2 → 0.45, gap 3 → 0.2, gap 4+ → 0.1
    let proximity = match *gap {
        0 => 1.0,
        1 => 0.7,
        2 => 0.45,
        3 => 0.2,
        _ => 0.1,
    };
    (Some(target), proximity)
}

/// OpenCode harvester. Reads `part` rows for the session out of the
/// shared `opencode.db`, groups them by `message_id` into
/// [`AssistantEvent`]s (one per assistant message), then runs the same
/// scan + classify + target pipeline as Claude.
///
/// # Schema notes
///
/// `part.data` is a JSON blob whose `type` field discriminates: `text`
/// parts carry assistant prose under `data.text`; `tool` parts carry
/// file targets under `data.state.input.filePath` (matching the
/// transcript loader's existing extractor). `step-start`, `step-finish`,
/// `patch`, and other system parts are ignored.
///
/// The DB is opened **read-only** so this is safe to run while
/// `opencode` itself is writing. Errors are surfaced as `IngestError`
/// rather than logged-and-swallowed: the caller (the reasoning pipeline)
/// already wraps each session in its own error path, so a per-session
/// SQLite hiccup doesn't sink the whole import.
fn harvest_opencode(
    db_path: &Path,
    session_id: &str,
    params: &HarvestParams,
) -> crate::Result<Vec<HarvestedCandidate>> {
    use rusqlite::{Connection, OpenFlags};
    let uri = format!("file:{}?mode=ro", db_path.display());
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    let conn = Connection::open_with_flags(&uri, flags).map_err(|e| {
        IngestError::Other(format!("opening opencode db {}: {e}", db_path.display()))
    })?;
    let _ = conn.busy_timeout(std::time::Duration::from_millis(500));

    // Order by `(message_id, time_created, id)` so all parts of a single
    // assistant message arrive contiguously and within the message in the
    // order they were emitted. That mirrors Claude's `content[]` layout.
    let mut stmt = conn
        .prepare(
            "SELECT message_id, time_created, data \
             FROM part \
             WHERE session_id = ?1 \
             ORDER BY message_id ASC, time_created ASC, id ASC",
        )
        .map_err(|e| IngestError::Other(format!("opencode part stmt: {e}")))?;
    let rows = stmt
        .query_map([session_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(|e| IngestError::Other(format!("opencode part query: {e}")))?;

    let mut events: Vec<AssistantEvent> = Vec::new();
    let mut current_msg: Option<String> = None;
    let mut current_event: Option<AssistantEvent> = None;
    for row in rows {
        let (msg_id, ts_ms, data) =
            row.map_err(|e| IngestError::Other(format!("opencode part row: {e}")))?;
        if current_msg.as_ref() != Some(&msg_id) {
            // Boundary between messages — emit the in-flight event if it
            // carried any signal and start a new one.
            if let Some(evt) = current_event.take()
                && (!evt.texts.is_empty() || !evt.files.is_empty())
            {
                events.push(evt);
            }
            current_msg = Some(msg_id.clone());
            current_event = Some(AssistantEvent {
                timestamp: chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ts_ms)
                    .unwrap_or_else(chrono::Utc::now),
                texts: Vec::new(),
                files: Vec::new(),
            });
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) else {
            continue;
        };
        let event = current_event.as_mut().expect("set above on boundary");
        match v.get("type").and_then(|x| x.as_str()) {
            Some("text") => {
                if let Some(text) = v.get("text").and_then(|x| x.as_str())
                    && !text.trim().is_empty()
                {
                    event.texts.push(text.to_string());
                }
            }
            Some("tool") => {
                // Reuse the same path-extraction logic as the loader,
                // but inline it here to keep this module decoupled from
                // `transcript::opencode`'s privates.
                if let Some(p) = v.pointer("/state/input/filePath").and_then(|x| x.as_str()) {
                    event.files.push(PathBuf::from(p));
                }
            }
            _ => {} // step-start/step-finish/patch/etc.
        }
    }
    if let Some(evt) = current_event
        && (!evt.texts.is_empty() || !evt.files.is_empty())
    {
        events.push(evt);
    }

    let out = harvest_from_events(&events, params);
    debug!(
        db = %db_path.display(),
        session_id = %session_id,
        events = events.len(),
        candidates = out.len(),
        "opencode harvest"
    );
    Ok(out)
}

/// Codex harvester. Walks a rollout `.jsonl`, normalizes its
/// `response_item` stream into [`AssistantEvent`]s, and runs the same
/// scan + classify + target pipeline as Claude/OpenCode.
///
/// # Mapping Codex's shape to ours
///
/// Codex serializes one event per JSONL line; an "event" in Codex
/// corresponds to either a single assistant `message` (text-only) or a
/// single tool invocation (`function_call`), not the bundled
/// `text + tool_use` blocks Claude emits. Two semantic consequences:
///
/// 1. **Text and file-touch live in adjacent events**, not the same
///    event. The harvester's lookahead-1 default (already tuned for
///    Claude's "explain then edit" cadence) picks this up correctly:
///    a text in event-N pairs with a `function_call` in event-N+1
///    at proximity 0.7. Same-event pairing (proximity 1.0) doesn't
///    apply for Codex.
/// 2. **`reasoning` events are server-redacted** — `content` is `null`
///    and `encrypted_content` is opaque. We skip them. There's nothing
///    we can extract that we don't already see in `message` events.
///
/// `cwd` is tracked across `session_meta` and `turn_context` events so
/// shell tokens that resolve relative to the current workdir get
/// stamped against the right path.
fn harvest_codex(path: &Path, params: &HarvestParams) -> crate::Result<Vec<HarvestedCandidate>> {
    use serde_json::Value;
    let text = std::fs::read_to_string(path).map_err(|e| {
        IngestError::Io(std::io::Error::new(
            e.kind(),
            format!("reading codex rollout {}: {e}", path.display()),
        ))
    })?;

    let mut current_cwd: Option<PathBuf> = None;
    let mut events: Vec<AssistantEvent> = Vec::new();

    for raw in text.lines() {
        if raw.trim().is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<CodexRawEvent>(raw) else {
            continue;
        };
        let Some(ts) = event.timestamp else {
            continue;
        };

        match event.event_type.as_deref() {
            Some("session_meta") => {
                if let Some(p) = event.payload.as_ref()
                    && current_cwd.is_none()
                    && let Some(c) = p.get("cwd").and_then(Value::as_str)
                {
                    current_cwd = Some(PathBuf::from(c));
                }
            }
            Some("turn_context") => {
                // Mid-session workdir switch — later commands resolve
                // against this cwd.
                if let Some(p) = event.payload.as_ref()
                    && let Some(c) = p.get("cwd").and_then(Value::as_str)
                {
                    current_cwd = Some(PathBuf::from(c));
                }
            }
            Some("response_item") => {
                let Some(p) = event.payload.as_ref() else {
                    continue;
                };
                let payload_type = p.get("type").and_then(Value::as_str).unwrap_or("");
                match payload_type {
                    "message" => {
                        // Only assistant turns carry harvest-able prose.
                        // User and developer messages are inputs we don't
                        // want to mistake for the model's reasoning.
                        if p.get("role").and_then(Value::as_str) != Some("assistant") {
                            continue;
                        }
                        let texts = collect_codex_message_texts(p);
                        if !texts.is_empty() {
                            events.push(AssistantEvent {
                                timestamp: ts,
                                texts,
                                files: Vec::new(),
                            });
                        }
                    }
                    "function_call" => {
                        // Only `exec_command` ever resolves to file
                        // touches; the loader uses the same gate.
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
                        // Per-call workdir override falls back to the
                        // session's running cwd.
                        let base_cwd: Option<PathBuf> = args_json
                            .get("workdir")
                            .and_then(Value::as_str)
                            .map(PathBuf::from)
                            .or_else(|| current_cwd.clone());
                        let mut touches = Vec::new();
                        crate::transcript::codex::extract_shell_touches(
                            cmd,
                            ts,
                            base_cwd.as_deref(),
                            &mut touches,
                        );
                        let files: Vec<PathBuf> = touches.into_iter().map(|t| t.path).collect();
                        if !files.is_empty() {
                            events.push(AssistantEvent {
                                timestamp: ts,
                                texts: Vec::new(),
                                files,
                            });
                        }
                    }
                    "custom_tool_call" => {
                        let name = p.get("name").and_then(Value::as_str).unwrap_or("");
                        if name != "apply_patch" {
                            continue;
                        }
                        let Some(input) = p.get("input").and_then(Value::as_str) else {
                            continue;
                        };
                        let mut touches = Vec::new();
                        crate::transcript::codex::extract_shell_touches(
                            input,
                            ts,
                            current_cwd.as_deref(),
                            &mut touches,
                        );
                        let files: Vec<PathBuf> = touches.into_iter().map(|t| t.path).collect();
                        if !files.is_empty() {
                            events.push(AssistantEvent {
                                timestamp: ts,
                                texts: Vec::new(),
                                files,
                            });
                        }
                    }
                    // `reasoning` events: server-redacted, content=null.
                    // Anything else: not a harvestable shape.
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let out = harvest_from_events(&events, params);
    debug!(
        path = %path.display(),
        events = events.len(),
        candidates = out.len(),
        "codex harvest"
    );
    Ok(out)
}

/// Pull every `output_text` block out of a Codex assistant message
/// payload. Codex's content array can in principle carry other types
/// (`refusal`, future tool-result variants); we ignore those and only
/// surface the text the model actually showed the user.
fn collect_codex_message_texts(payload: &serde_json::Value) -> Vec<String> {
    let Some(content) = payload.get("content").and_then(|c| c.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(content.len());
    for block in content {
        if block.get("type").and_then(|t| t.as_str()) != Some("output_text") {
            continue;
        }
        let Some(text) = block.get("text").and_then(|t| t.as_str()) else {
            continue;
        };
        if !text.trim().is_empty() {
            out.push(text.to_string());
        }
    }
    out
}

#[derive(Debug, serde::Deserialize)]
struct CodexRawEvent {
    #[serde(rename = "type")]
    event_type: Option<String>,
    timestamp: Option<DateTime<Utc>>,
    payload: Option<serde_json::Value>,
}

/// Public hook for a later Codex reader — kept as a named no-op so
/// downstream callers can feature-detect.
#[doc(hidden)]
pub fn harvest_from_codex_events(_path: &Path) -> Vec<HarvestedCandidate> {
    Vec::new()
}

// ---------- Keep: trim + score + seal ----------

/// Apply stage 2 to one candidate. Returns `Some(point)` when kept,
/// `None` when dropped. Callers typically `filter_map` over the harvest
/// output.
pub fn keep(
    cand: &HarvestedCandidate,
    evidence: ReasoningEvidence,
    params: &KeepParams,
) -> Option<ReasoningPoint> {
    let trimmed = trim_to_notecard(&cand.text, params.max_chars)?;
    let lower = cand.text.to_ascii_lowercase();

    // Filter A: transient bug narrative. "X is failing because Y",
    // "the build broke because…" — these describe a bug the agent
    // hit, not durable knowledge. Skip.
    if is_bug_narrative(&lower) {
        return None;
    }

    // Filter B: Rule-voice check. A trigger like "never " or "always "
    // maps to Rule only when the sentence is imperative (starts with
    // the keyword or "you "/"don't "/"avoid "). Descriptive uses like
    // "the effect never fires" are Why, not Rule, and should not
    // impersonate a prescriptive rule.
    let (kind, trigger_strength) = reclassify(cand, &lower);

    let target_factor = if cand.target_hint.is_some() {
        cand.target_proximity.max(0.1)
    } else {
        params.no_target_penalty
    };
    // Weights: trigger strength is the primary signal, target
    // proximity lifts or dampens it. Length fit is a soft bonus for
    // sentences that fit the notecard without truncation.
    let length_fit = if cand.text.chars().count() <= params.max_chars {
        1.0
    } else {
        0.8
    };
    let confidence = (0.6 * trigger_strength + 0.3 * target_factor + 0.1 * length_fit).min(1.0);
    if confidence < params.keep_threshold {
        return None;
    }
    // A candidate with neither target nor a strong trigger is almost
    // always noise — gate it explicitly. Without this check a weak
    // "note that …" sentence with a lookahead target scraped from an
    // unrelated edit can squeak past the threshold.
    if cand.target_hint.is_none() && trigger_strength < 0.7 {
        return None;
    }

    let target = cand.target_hint.clone().unwrap_or(ReasoningTarget {
        file: String::new(),
        symbol: None,
        line_range: None,
    });
    let point = ReasoningPoint {
        kind,
        text: trimmed,
        target,
        evidence,
        confidence,
    };
    point.is_well_formed().then_some(point)
}

/// Matches sentences that are complaining about the current build,
/// test suite, or error state. These are common in agent narration
/// but carry no durable lesson — the fix probably lives in the same
/// commit, so the diff already tells the story.
fn is_bug_narrative(lower: &str) -> bool {
    // The hallmark: a failure verb paired with `because` (or similar).
    // Pure failure mentions without the "because" hinge are usually
    // progress reports ("the tests are still failing"), which we also
    // don't want to annotate with.
    const FAIL_WORDS: &[&str] = &[
        "failing",
        " failed",
        " fails",
        " fail because",
        " broke",
        " breaks",
        " broken",
        "errored",
        "errors out",
        "doesn't compile",
        "won't compile",
        "regressed",
        "panics because",
        "isn't set",
        "not set in",
    ];
    if FAIL_WORDS.iter().any(|w| lower.contains(w)) {
        return true;
    }
    // Self-directed investigation narration is almost never durable
    // knowledge. "Let me check...", "I'll look into..." etc describe
    // the agent's next action, not an insight about the code.
    const NARRATION_PREFIXES: &[&str] = &[
        // `let me ` covers "let me fix", "let me check", "let me
        // restructure", etc. Almost all "let me …" openers are
        // self-directed intent statements, not durable insights.
        "let me ",
        "i'll ",
        "i need to ",
        "i'm going to ",
        "now let's ",
        "now i'll ",
    ];
    let head = lower.trim_start();
    NARRATION_PREFIXES.iter().any(|p| head.starts_with(p))
}

/// If a "never "/"always " trigger fired but the sentence isn't in
/// imperative voice (doesn't start with the trigger word, "you ",
/// "don't ", "avoid "), downgrade it to [`AnnotationKind::Rationale`]
/// with a reduced trigger strength. Other triggers pass through unchanged.
fn reclassify(cand: &HarvestedCandidate, lower: &str) -> (AnnotationKind, f32) {
    let base_strength = TRIGGERS
        .iter()
        .find(|t| t.needle == cand.trigger)
        .map(|t| t.strength)
        .unwrap_or(0.5);

    let is_invariant_trigger = matches!(cand.trigger, "never " | "always ");
    if !is_invariant_trigger {
        return (cand.kind_hint, base_strength);
    }

    let sentence_lower = lower.trim_start();
    // Imperative voice starts with the trigger word, "you must",
    // "don't", "avoid", or "do not".
    let imperative = sentence_lower.starts_with(cand.trigger)
        || sentence_lower.starts_with("you ")
        || sentence_lower.starts_with("don't ")
        || sentence_lower.starts_with("do not ")
        || sentence_lower.starts_with("avoid ")
        || sentence_lower.starts_with("must ");

    if imperative {
        (AnnotationKind::Invariant, base_strength)
    } else {
        // Descriptive — "the effect never fires", "X always runs
        // after Y". Demote to Rationale at reduced strength so it
        // still has to clear the keep threshold.
        (AnnotationKind::Rationale, base_strength * 0.55)
    }
}

/// Trim a sentence to fit the notecard budget. Tries three increasingly-
/// loose cut sites in order, taking the first that lands past `max_chars / 2`:
///
/// 1. **Sentence boundary** — `.`/`!`/`?` followed by a space. A single
///    leading sentence often carries the whole insight; a clean cut here
///    reads as a complete thought, not a truncation.
/// 2. **Clause boundary** — `,`/`;`/`:` followed by a space. Better than
///    a bare word break for keeping the phrase scannable.
/// 3. **Word boundary** — last whitespace before `max_chars`. The fallback
///    when neither punctuation works.
///
/// Returns `None` only when the input is whitespace-only after `trim()`.
fn trim_to_notecard(s: &str, max_chars: usize) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() <= max_chars {
        return Some(trimmed.to_string());
    }

    // Pre-compute the (char_index, char) sequence once. We need char-level
    // boundaries (not byte indexes) so multibyte UTF-8 doesn't bisect a
    // codepoint, but we also need byte indexes to slice the string.
    //
    // `byte_at[i]` = byte index where the i-th character starts; we sentinel
    // the end with `trimmed.len()` so `byte_at[k]` is always meaningful for
    // any cut point k in 0..=char_count.
    let mut byte_at: Vec<usize> = Vec::with_capacity(trimmed.len() + 1);
    for (b, _c) in trimmed.char_indices() {
        byte_at.push(b);
    }
    byte_at.push(trimmed.len());

    let total_chars = byte_at.len() - 1;
    let cap = max_chars.min(total_chars);
    let half = max_chars / 2;

    // Walk the prefix once, recording the latest occurrence of each cut
    // site within `cap` chars. The "boundary" rule everywhere is "this
    // punctuation followed by whitespace" — that way `e.g.` and decimal
    // numbers don't trigger a sentence cut.
    let bytes = trimmed.as_bytes();
    let mut last_sentence: Option<usize> = None;
    let mut last_clause: Option<usize> = None;
    let mut last_space: Option<usize> = None;

    let chars: Vec<char> = trimmed.chars().take(cap + 1).collect();
    for i in 0..cap {
        let c = chars[i];
        if c.is_whitespace() {
            last_space = Some(i);
        }
        // Look at the next char to enforce the "followed by whitespace"
        // rule. At i == cap-1 the next char is outside our window — treat
        // it as a non-match (we'd rather not split at the very edge).
        let next_is_ws = chars.get(i + 1).is_some_and(|n: &char| n.is_whitespace());
        if next_is_ws {
            match c {
                '.' | '!' | '?' => last_sentence = Some(i + 1),
                ',' | ';' | ':' => last_clause = Some(i + 1),
                _ => {}
            }
        }
    }

    let cut_at = last_sentence
        .filter(|&i| i > half)
        .or_else(|| last_clause.filter(|&i| i > half))
        .or_else(|| last_space.filter(|&i| i > half))
        .unwrap_or(cap);

    let cut_byte = byte_at[cut_at];
    let out = bytes[..cut_byte].to_vec();
    // Safe: we cut at a char boundary (byte_at indexes char starts).
    let out = String::from_utf8(out).expect("char-boundary cut yields utf8");
    let out = out.trim_end_matches(char::is_whitespace).to_string();
    if out.is_empty() { None } else { Some(out) }
}

// ---------- One-shot convenience ----------

/// Run both stages and seal the surviving points against the supplied
/// `commit_sha`. This is the entry point the importer calls per
/// matched (transcript, commit) pair.
///
/// Callers who want raw candidates for tooling can call [`harvest`] and
/// [`keep`] separately.
pub fn extract(
    t: &Transcript,
    commit_sha: &str,
    harvest_params: &HarvestParams,
    keep_params: &KeepParams,
) -> crate::Result<Vec<ReasoningPoint>> {
    let candidates = harvest(t, harvest_params)?;
    let mut out: Vec<ReasoningPoint> = Vec::with_capacity(candidates.len());
    // Dedupe by (text, target.file) — Claude occasionally restates the
    // same sentence across several assistant turns (quoting itself in a
    // wrap-up message, for example). Keep the first occurrence, which
    // carries the earliest turn index in evidence.
    let mut seen = std::collections::HashSet::<(String, String)>::new();
    for c in candidates {
        let evidence = ReasoningEvidence {
            session_id: t.session_id.clone(),
            // Turn index within the assistant event stream — good
            // enough for now; richer (first,last) ranges will come in
            // when we switch to multi-sentence candidates.
            turn_range: (c.turn_index, c.turn_index),
            commit_sha: commit_sha.to_string(),
            provider: t.provider.as_str().to_string(),
        };
        if let Some(p) = keep(&c, evidence, keep_params) {
            let key = (p.text.clone(), p.target.file.clone());
            if seen.insert(key) {
                out.push(p);
            }
        }
    }
    Ok(out)
}

// ---------- Internal raw-event shapes ----------

/// Provider-agnostic event record consumed by [`harvest_from_events`].
/// Both Claude (typed JSONL blocks) and OpenCode (SQLite parts grouped
/// by `message_id`) normalize to this shape so the scoring logic stays
/// in one place.
#[derive(Debug)]
struct AssistantEvent {
    timestamp: DateTime<Utc>,
    texts: Vec<String>,
    files: Vec<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct ClaudeRawEvent {
    #[serde(rename = "type")]
    event_type: Option<String>,
    timestamp: Option<DateTime<Utc>>,
    message: Option<ClaudeRawMessage>,
}

#[derive(Debug, Deserialize)]
struct ClaudeRawMessage {
    /// Polymorphic, mirroring [`super::transcript::claude::RawContent`].
    /// Most assistant turns are typed-block arrays; a non-trivial fraction
    /// (and almost every user turn) flatten to a plain string. We model
    /// both so the harvester doesn't drop events whose content is a
    /// string — a regression that lost roughly half of all assistant
    /// turns when the field was typed as `Vec<ClaudeRawBlock>`.
    content: Option<ClaudeRawContent>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ClaudeRawContent {
    Text(String),
    Blocks(Vec<ClaudeRawBlock>),
}

impl ClaudeRawContent {
    /// Empty slice for the string case so the harvest loop iterates
    /// uniformly. Strings carry no `tool_use` so they produce no
    /// candidates either way; we lose nothing by treating them as empty.
    fn blocks(&self) -> &[ClaudeRawBlock] {
        match self {
            ClaudeRawContent::Blocks(b) => b.as_slice(),
            ClaudeRawContent::Text(_) => &[],
        }
    }
}

#[derive(Debug, Deserialize)]
struct ClaudeRawBlock {
    #[serde(rename = "type")]
    block_type: Option<String>,
    text: Option<String>,
    input: Option<ClaudeRawToolInput>,
}

#[derive(Debug, Deserialize)]
struct ClaudeRawToolInput {
    file_path: Option<String>,
    notebook_path: Option<String>,
}

#[cfg(test)]
#[allow(clippy::useless_format, clippy::format_in_format_args)]
mod tests {
    use super::*;
    use crate::transcript::types::{FileTouch, TouchKind};

    fn base_ts() -> DateTime<Utc> {
        "2026-04-21T10:00:00Z".parse().unwrap()
    }

    fn write_session(text: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("sess.jsonl");
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    fn make_transcript(path: PathBuf) -> Transcript {
        Transcript {
            provider: Provider::Claude,
            session_id: "S".into(),
            source_path: path,
            cwd: Some(PathBuf::from("/repo")),
            started_at: base_ts(),
            ended_at: base_ts(),
            turn_count: 1,
            files_touched: vec![FileTouch {
                path: PathBuf::from("/repo/a.rs"),
                timestamp: base_ts(),
                kind: TouchKind::Write,
            }],
            starting_commit: None,
        }
    }

    #[test]
    fn split_sentences_respects_code_fences_and_periods() {
        let got = split_sentences("Use `a.b.c`. Never call it twice. Avoid the trap.");
        assert_eq!(
            got,
            vec![
                "Use `a.b.c`.".to_string(),
                "Never call it twice.".to_string(),
                "Avoid the trap.".to_string(),
            ]
        );
    }

    #[test]
    fn classify_picks_invariant_for_never() {
        let (t, _) = classify("never call token without a tenant").unwrap();
        assert_eq!(t.kind, AnnotationKind::Invariant);
    }

    #[test]
    fn classify_picks_rationale_for_because() {
        let (t, _) = classify("we do this because reentry breaks the lock").unwrap();
        assert_eq!(t.kind, AnnotationKind::Rationale);
    }

    #[test]
    fn classify_ignores_embedded_keywords() {
        // "Always" as part of a wider word should not fire.
        assert!(classify("the alwaysOn flag is a lie").is_none());
    }

    #[test]
    fn harvest_emits_rule_candidate_with_immediate_target() {
        let body = r#"{"type":"assistant","sessionId":"S","cwd":"/repo","timestamp":"2026-04-21T10:00:00Z","uuid":"a1","message":{"content":[{"type":"text","text":"Never call parseToken without a tenant scope."},{"type":"tool_use","name":"Edit","input":{"file_path":"/repo/token.rs","old_string":"x","new_string":"y"}}]}}"#;
        let (_d, path) = write_session(body);
        let t = make_transcript(path);
        let cands = harvest(&t, &HarvestParams::default()).unwrap();
        assert_eq!(cands.len(), 1);
        let c = &cands[0];
        assert_eq!(c.kind_hint, AnnotationKind::Invariant);
        assert_eq!(
            c.target_hint.as_ref().map(|t| t.file.as_str()),
            Some("/repo/token.rs")
        );
        assert!((c.target_proximity - 1.0).abs() < 1e-6);
    }

    #[test]
    fn harvest_skips_redacted_thinking_blocks() {
        // Real Claude exports leave `thinking` empty when the thought
        // is redacted at write-time — we should not fire keywords
        // against an empty string.
        let body = r#"{"type":"assistant","sessionId":"S","cwd":"/r","timestamp":"2026-04-21T10:00:00Z","uuid":"a","message":{"content":[{"type":"thinking","thinking":""}]}}"#;
        let (_d, path) = write_session(body);
        let t = make_transcript(path);
        let cands = harvest(&t, &HarvestParams::default()).unwrap();
        assert!(cands.is_empty());
    }

    #[test]
    fn harvest_uses_lookahead_for_target() {
        // Text block fires, but the Edit lives in the *next* assistant
        // event — target should still resolve with reduced proximity.
        let body = format!(
            "{}\n{}",
            r#"{"type":"assistant","sessionId":"S","cwd":"/r","timestamp":"2026-04-21T10:00:00Z","uuid":"a","message":{"content":[{"type":"text","text":"We avoid global state because reentry breaks."}]}}"#,
            r#"{"type":"assistant","sessionId":"S","cwd":"/r","timestamp":"2026-04-21T10:00:30Z","uuid":"b","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/r/state.rs","old_string":"x","new_string":"y"}}]}}"#
        );
        let (_d, path) = write_session(&body);
        let t = make_transcript(path);
        let cands = harvest(&t, &HarvestParams::default()).unwrap();
        assert_eq!(cands.len(), 1);
        // Gap-1 proximity, rounded.
        assert!((cands[0].target_proximity - 0.7).abs() < 1e-6);
    }

    #[test]
    fn keep_drops_weak_candidate_without_target() {
        let cand = HarvestedCandidate {
            turn_index: 0,
            text: "note that the config is loaded at startup".into(),
            kind_hint: AnnotationKind::Rationale,
            target_hint: None,
            target_proximity: 0.0,
            trigger: "note that ",
            when: base_ts(),
        };
        let ev = ReasoningEvidence {
            session_id: "S".into(),
            turn_range: (0, 0),
            commit_sha: "deadbeef".into(),
            provider: "claude".into(),
        };
        assert!(keep(&cand, ev, &KeepParams::default()).is_none());
    }

    #[test]
    fn keep_promotes_strong_candidate_with_target() {
        let cand = HarvestedCandidate {
            turn_index: 2,
            text: "Never call parseToken without a tenant scope.".into(),
            kind_hint: AnnotationKind::Invariant,
            target_hint: Some(ReasoningTarget {
                file: "crates/auth/src/token.rs".into(),
                symbol: None,
                line_range: None,
            }),
            target_proximity: 1.0,
            trigger: "never ",
            when: base_ts(),
        };
        let ev = ReasoningEvidence {
            session_id: "S".into(),
            turn_range: (2, 2),
            commit_sha: "abc".into(),
            provider: "claude".into(),
        };
        let p = keep(&cand, ev, &KeepParams::default()).unwrap();
        assert_eq!(p.kind, AnnotationKind::Invariant);
        assert!(p.confidence >= 0.55);
        assert_eq!(p.target.file, "crates/auth/src/token.rs");
    }

    #[test]
    fn trim_to_notecard_cuts_at_word_boundary() {
        let long = "a ".repeat(100); // 200 chars
        let got = trim_to_notecard(&long, 140).unwrap();
        assert!(got.chars().count() <= 140);
        assert!(!got.ends_with(' '));
    }

    #[test]
    fn extract_end_to_end_seals_evidence() {
        let body = r#"{"type":"assistant","sessionId":"S","cwd":"/repo","timestamp":"2026-04-21T10:00:00Z","uuid":"a1","message":{"content":[{"type":"text","text":"Never call parseToken without a tenant scope."},{"type":"tool_use","name":"Edit","input":{"file_path":"/repo/token.rs","old_string":"x","new_string":"y"}}]}}"#;
        let (_d, path) = write_session(body);
        let t = make_transcript(path);
        let points = extract(
            &t,
            "deadbeef",
            &HarvestParams::default(),
            &KeepParams::default(),
        )
        .unwrap();
        assert_eq!(points.len(), 1);
        let p = &points[0];
        assert_eq!(p.evidence.commit_sha, "deadbeef");
        assert_eq!(p.evidence.provider, "claude");
        assert_eq!(p.evidence.session_id, "S");
    }

    #[test]
    fn harvest_prefers_sentence_mentioned_basename_over_adjacent_tool_use() {
        // Regression: previously every sentence in an event was paired
        // with whichever file the same event happened to edit. Mixed-
        // topic turns ("fixed Combobox styling … patched auth.rs") would
        // attach the UI prose to auth.rs. With sentence-level path
        // scanning, a sentence that names `Combobox.svelte` should
        // resolve to that file even when the immediate tool_use is on
        // an unrelated path.
        let body = format!(
            "{{\"type\":\"assistant\",\"sessionId\":\"S\",\"cwd\":\"/r\",\
            \"timestamp\":\"2026-04-21T10:00:00Z\",\"uuid\":\"a\",\
            \"message\":{{\"content\":[\
            {{\"type\":\"text\",\"text\":\"Never call parseToken before tenant scope is loaded in Combobox.svelte.\"}},\
            {{\"type\":\"tool_use\",\"name\":\"Edit\",\"input\":{{\"file_path\":\"/r/auth.rs\",\"old_string\":\"x\",\"new_string\":\"y\"}}}},\
            {{\"type\":\"tool_use\",\"name\":\"Edit\",\"input\":{{\"file_path\":\"/r/Combobox.svelte\",\"old_string\":\"a\",\"new_string\":\"b\"}}}}\
            ]}}}}"
        );
        let (_d, path) = write_session(&body);
        let t = make_transcript(path);
        let cands = harvest(&t, &HarvestParams::default()).unwrap();
        assert_eq!(cands.len(), 1, "got: {cands:?}");
        let c = &cands[0];
        assert_eq!(
            c.target_hint.as_ref().map(|t| t.file.as_str()),
            Some("/r/Combobox.svelte"),
            "sentence-mentioned path should win over the first tool_use"
        );
        // In-text matches always score 1.0 proximity.
        assert!((c.target_proximity - 1.0).abs() < 1e-6);
    }

    #[test]
    fn harvest_falls_back_to_adjacent_tool_use_when_no_path_in_text() {
        // No file path mentioned in the prose → the immediate same-event
        // tool_use is still used as the target (gap=0 proximity 1.0).
        let body = r#"{"type":"assistant","sessionId":"S","cwd":"/r","timestamp":"2026-04-21T10:00:00Z","uuid":"a","message":{"content":[{"type":"text","text":"Never call this without a tenant scope."},{"type":"tool_use","name":"Edit","input":{"file_path":"/r/token.rs","old_string":"x","new_string":"y"}}]}}"#;
        let (_d, path) = write_session(body);
        let t = make_transcript(path);
        let cands = harvest(&t, &HarvestParams::default()).unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(
            cands[0].target_hint.as_ref().map(|t| t.file.as_str()),
            Some("/r/token.rs")
        );
    }

    #[test]
    fn trim_to_notecard_prefers_sentence_then_clause_then_word() {
        // Sentence boundary preferred when present in the budget. The
        // candidate has multiple sentences; the trimmer should land on
        // the period after the first one, returning a clean line —
        // not "…so the worktree stays" with no terminator.
        let s = "We rename the branch in place. The worktree keeps its working tree intact this way, no copy required.";
        let got = trim_to_notecard(s, 50).unwrap();
        assert_eq!(got, "We rename the branch in place.");

        // No sentence boundary inside the budget → fall back to clause
        // boundary (comma). The trimmer cuts immediately *after* the
        // comma so the remaining text reads as a complete clause unit
        // — "We rename the branch in place," — rather than chopping a
        // trailing word.
        let s = "We rename the branch in place, keeping the worktree pointed at it the entire time without breaking anyone";
        let got = trim_to_notecard(s, 50).unwrap();
        assert!(
            got.ends_with("place,") || got.ends_with("place"),
            "expected clause boundary near `place`, got {got:?}"
        );

        // No punctuation at all → word boundary fallback (existing behavior).
        let s = "rename the branch in place keeping the worktree pointed at it the entire time without breaking anyone";
        let got = trim_to_notecard(s, 50).unwrap();
        assert!(
            !got.ends_with(' '),
            "should not have trailing space, got {got:?}"
        );
        assert!(got.chars().count() <= 50);
    }

    #[test]
    fn keep_drops_bug_narrative() {
        // "The tests are all failing because X" is a progress report,
        // not durable knowledge — it should be filtered even though
        // the " because " trigger fires and there's a strong target.
        let cand = HarvestedCandidate {
            turn_index: 4,
            text: "The tests are all failing because the hosted subcommand was removed.".into(),
            kind_hint: AnnotationKind::Rationale,
            target_hint: Some(ReasoningTarget {
                file: "crates/cli/Cargo.toml".into(),
                symbol: None,
                line_range: None,
            }),
            target_proximity: 1.0,
            trigger: " because ",
            when: base_ts(),
        };
        let ev = ReasoningEvidence {
            session_id: "S".into(),
            turn_range: (4, 4),
            commit_sha: "abc".into(),
            provider: "claude".into(),
        };
        assert!(keep(&cand, ev, &KeepParams::default()).is_none());
    }

    #[test]
    fn keep_drops_self_directed_narration() {
        // "Let me check..." prefixes describe the agent's intent, not
        // durable knowledge — filter them out before they reach scoring.
        let cand = HarvestedCandidate {
            turn_index: 1,
            text: "Let me check because the previous state was fine.".into(),
            kind_hint: AnnotationKind::Rationale,
            target_hint: Some(ReasoningTarget {
                file: "Cargo.toml".into(),
                symbol: None,
                line_range: None,
            }),
            target_proximity: 1.0,
            trigger: " because ",
            when: base_ts(),
        };
        let ev = ReasoningEvidence {
            session_id: "S".into(),
            turn_range: (1, 1),
            commit_sha: "c".into(),
            provider: "claude".into(),
        };
        assert!(keep(&cand, ev, &KeepParams::default()).is_none());
    }

    #[test]
    fn keep_downgrades_descriptive_never_to_rationale() {
        // "The effect never re-ran" is descriptive, not prescriptive.
        // Should be reclassified as Rationale and re-scored under the
        // descriptive-strength penalty.
        let cand = HarvestedCandidate {
            turn_index: 9,
            text: "The effect never re-ran on theme toggle.".into(),
            kind_hint: AnnotationKind::Invariant,
            target_hint: Some(ReasoningTarget {
                file: "web/src/routes/+page.svelte".into(),
                symbol: None,
                line_range: None,
            }),
            target_proximity: 1.0,
            trigger: "never ",
            when: base_ts(),
        };
        let ev = ReasoningEvidence {
            session_id: "S".into(),
            turn_range: (9, 9),
            commit_sha: "def".into(),
            provider: "claude".into(),
        };
        // With the descriptive penalty: 0.6 * (0.8 * 0.55) + 0.3 * 1.0 + 0.1 * 1.0
        // = 0.264 + 0.3 + 0.1 = 0.664 — above the 0.55 default threshold,
        // but the kind is now Rationale, not Invariant.
        let p = keep(&cand, ev, &KeepParams::default()).unwrap();
        assert_eq!(p.kind, AnnotationKind::Rationale);
    }

    #[test]
    fn keep_preserves_imperative_never_as_invariant() {
        // Imperative voice — the sentence starts with "Never " — keeps
        // the Invariant classification at full strength.
        let cand = HarvestedCandidate {
            turn_index: 0,
            text: "Never call parseToken without a tenant scope.".into(),
            kind_hint: AnnotationKind::Invariant,
            target_hint: Some(ReasoningTarget {
                file: "src/token.rs".into(),
                symbol: None,
                line_range: None,
            }),
            target_proximity: 1.0,
            trigger: "never ",
            when: base_ts(),
        };
        let ev = ReasoningEvidence {
            session_id: "S".into(),
            turn_range: (0, 0),
            commit_sha: "a".into(),
            provider: "claude".into(),
        };
        let p = keep(&cand, ev, &KeepParams::default()).unwrap();
        assert_eq!(p.kind, AnnotationKind::Invariant);
    }

    #[test]
    fn opencode_harvest_extracts_invariant_with_target() {
        // End-to-end: build a fixture opencode.db with one session that
        // has one assistant message containing a `text` part (reasoning
        // prose) and a following `tool` part naming a file. The
        // harvester should classify the sentence as `Invariant` and
        // bind it to the tool's filePath.
        use rusqlite::{Connection, params};
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("opencode.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL,
                 directory TEXT NOT NULL, title TEXT NOT NULL, version TEXT NOT NULL,
                 time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                 time_created INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL,
                 session_id TEXT NOT NULL, time_created INTEGER NOT NULL,
                 data TEXT NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES ('S','p','/repo','t','v',0,0)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO message VALUES ('M','S',1000,'{}')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO part VALUES (?1, 'M', 'S', ?2, ?3)",
            params![
                "p1",
                1000_i64,
                r#"{"type":"text","text":"Never call parseToken without a tenant scope."}"#
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES (?1, 'M', 'S', ?2, ?3)",
            params![
                "p2",
                1500_i64,
                r#"{"type":"tool","tool":"edit","state":{"input":{"filePath":"/repo/token.rs"}}}"#
            ],
        )
        .unwrap();
        drop(conn);

        let cands = harvest_opencode(&db_path, "S", &HarvestParams::default()).unwrap();
        assert_eq!(cands.len(), 1, "got: {cands:?}");
        let c = &cands[0];
        assert_eq!(c.kind_hint, AnnotationKind::Invariant);
        assert_eq!(
            c.target_hint.as_ref().map(|t| t.file.as_str()),
            Some("/repo/token.rs")
        );
    }

    #[test]
    fn opencode_harvest_groups_parts_by_message() {
        // Two messages in one session, each with its own text+tool. The
        // harvester must NOT cross-attribute message-1's prose to
        // message-2's tool — they should resolve as separate events
        // with separate target picks (path-mention preferred but here
        // sentences don't name files, so same-event tool wins).
        use rusqlite::{Connection, params};
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("opencode.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL,
                 directory TEXT NOT NULL, title TEXT NOT NULL, version TEXT NOT NULL,
                 time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                 time_created INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL,
                 session_id TEXT NOT NULL, time_created INTEGER NOT NULL,
                 data TEXT NOT NULL);
             INSERT INTO session VALUES ('S','p','/repo','t','v',0,0);
             INSERT INTO message VALUES ('M1','S',1000,'{}');
             INSERT INTO message VALUES ('M2','S',2000,'{}');",
        )
        .unwrap();
        // Message 1: "always …" + edit on a.rs
        conn.execute(
            "INSERT INTO part VALUES (?1,?2,'S',?3,?4)",
            params![
                "p1",
                "M1",
                1000_i64,
                r#"{"type":"text","text":"Always lock the mutex before reading the buffer."}"#
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES (?1,?2,'S',?3,?4)",
            params![
                "p2",
                "M1",
                1100_i64,
                r#"{"type":"tool","tool":"edit","state":{"input":{"filePath":"/repo/a.rs"}}}"#
            ],
        )
        .unwrap();
        // Message 2: "never …" + edit on b.rs
        conn.execute(
            "INSERT INTO part VALUES (?1,?2,'S',?3,?4)",
            params![
                "p3",
                "M2",
                2000_i64,
                r#"{"type":"text","text":"Never close the connection without flushing first."}"#
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES (?1,?2,'S',?3,?4)",
            params![
                "p4",
                "M2",
                2100_i64,
                r#"{"type":"tool","tool":"edit","state":{"input":{"filePath":"/repo/b.rs"}}}"#
            ],
        )
        .unwrap();
        drop(conn);

        let cands = harvest_opencode(&db_path, "S", &HarvestParams::default()).unwrap();
        assert_eq!(cands.len(), 2, "got: {cands:?}");
        // Each candidate's target should match its OWN message's tool.
        let by_text: std::collections::HashMap<&str, &str> = cands
            .iter()
            .map(|c| {
                (
                    c.text.as_str(),
                    c.target_hint.as_ref().unwrap().file.as_str(),
                )
            })
            .collect();
        assert_eq!(
            by_text.get("Always lock the mutex before reading the buffer."),
            Some(&"/repo/a.rs"),
            "by_text: {by_text:?}"
        );
        assert_eq!(
            by_text.get("Never close the connection without flushing first."),
            Some(&"/repo/b.rs"),
            "by_text: {by_text:?}"
        );
    }

    #[test]
    fn codex_empty_session_yields_no_candidates() {
        // No events → no candidates. Mirrors the loader's "session
        // without timestamps" behaviour.
        let (_d, path) = write_session("{}");
        let mut t = make_transcript(path);
        t.provider = Provider::Codex;
        let cands = harvest(&t, &HarvestParams::default()).unwrap();
        assert!(cands.is_empty());
    }

    #[test]
    fn codex_assistant_message_then_function_call_pairs_via_lookahead() {
        // Codex's narrate-then-act cadence: the assistant emits a
        // `message` event with reasoning prose, then in the next event a
        // `function_call` invokes `exec_command` to apply the patch the
        // prose just described. With lookahead=1 (the default) the text
        // event pairs with the next-event tool target at proximity 0.7.
        let cmd = "apply_patch <<'P'\n*** Begin Patch\n*** Update File: src/auth.rs\n@@\n-old\n+new\n*** End Patch\nP";
        let args = serde_json::json!({"cmd": cmd, "workdir": "/repo"}).to_string();
        let body = format!(
            "{}\n{}\n{}",
            r#"{"timestamp":"2026-04-21T10:00:00Z","type":"session_meta","payload":{"id":"S","cwd":"/repo"}}"#,
            r#"{"timestamp":"2026-04-21T10:00:30Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Never mutate the global session lock without first acquiring the tenant scope."}]}}"#,
            format!(
                r#"{{"timestamp":"2026-04-21T10:01:00Z","type":"response_item","payload":{{"type":"function_call","name":"exec_command","arguments":{}}}}}"#,
                serde_json::to_string(&args).unwrap()
            ),
        );
        let (_d, path) = write_session(&body);
        let mut t = make_transcript(path);
        t.provider = Provider::Codex;
        let cands = harvest(&t, &HarvestParams::default()).unwrap();
        assert_eq!(cands.len(), 1, "got: {cands:?}");
        let c = &cands[0];
        assert_eq!(c.kind_hint, AnnotationKind::Invariant); // "Never " trigger
        assert_eq!(
            c.target_hint.as_ref().map(|t| t.file.as_str()),
            Some("/repo/src/auth.rs"),
            "lookahead-1 should pair text with the next event's tool target"
        );
        // Same-event proximity is 1.0; lookahead-1 is 0.7.
        assert!((c.target_proximity - 0.7).abs() < 1e-6);
    }

    #[test]
    fn codex_assistant_message_then_custom_apply_patch_pairs_via_lookahead() {
        let patch = "*** Begin Patch\n\
*** Update File: src/native.rs\n\
@@\n\
-old\n\
+new\n\
*** End Patch\n";
        let body = format!(
            "{}\n{}\n{}",
            r#"{"timestamp":"2026-04-21T10:00:00Z","type":"session_meta","payload":{"id":"S","cwd":"/repo"}}"#,
            r#"{"timestamp":"2026-04-21T10:00:30Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Always validate native.rs before updating the generated index."}]}}"#,
            format!(
                r#"{{"timestamp":"2026-04-21T10:01:00Z","type":"response_item","payload":{{"type":"custom_tool_call","status":"completed","name":"apply_patch","input":{}}}}}"#,
                serde_json::to_string(patch).unwrap()
            ),
        );
        let (_d, path) = write_session(&body);
        let mut t = make_transcript(path);
        t.provider = Provider::Codex;
        let cands = harvest(&t, &HarvestParams::default()).unwrap();
        assert_eq!(cands.len(), 1, "got: {cands:?}");
        let c = &cands[0];
        assert_eq!(
            c.target_hint.as_ref().map(|t| t.file.as_str()),
            Some("/repo/src/native.rs"),
        );
        assert!((c.target_proximity - 1.0).abs() < 1e-6);
    }

    #[test]
    fn codex_redacted_reasoning_blocks_produce_no_candidates() {
        // The `reasoning` payload type carries `content: null` and an
        // opaque `encrypted_content` string. Even when paired with a
        // following file edit there's no text to harvest from — so the
        // harvester must produce zero candidates rather than try to
        // mine the encrypted blob.
        let cmd = "apply_patch <<'P'\n*** Begin Patch\n*** Update File: src/x.rs\n@@\n-a\n+b\n*** End Patch\nP";
        let args = serde_json::json!({"cmd": cmd, "workdir": "/repo"}).to_string();
        let body = format!(
            "{}\n{}\n{}",
            r#"{"timestamp":"2026-04-21T10:00:00Z","type":"session_meta","payload":{"id":"S","cwd":"/repo"}}"#,
            r#"{"timestamp":"2026-04-21T10:00:30Z","type":"response_item","payload":{"type":"reasoning","summary":[],"content":null,"encrypted_content":"gAAAA..."}}"#,
            format!(
                r#"{{"timestamp":"2026-04-21T10:01:00Z","type":"response_item","payload":{{"type":"function_call","name":"exec_command","arguments":{}}}}}"#,
                serde_json::to_string(&args).unwrap()
            ),
        );
        let (_d, path) = write_session(&body);
        let mut t = make_transcript(path);
        t.provider = Provider::Codex;
        let cands = harvest(&t, &HarvestParams::default()).unwrap();
        assert!(cands.is_empty(), "got: {cands:?}");
    }

    #[test]
    fn codex_user_messages_are_not_treated_as_reasoning() {
        // User and developer messages in Codex carry their input
        // (instructions, prompts) — not the agent's reasoning. The
        // harvester must only mine assistant-role messages or it'll
        // attribute user-typed instructions to the model.
        let body = format!(
            "{}\n{}",
            r#"{"timestamp":"2026-04-21T10:00:00Z","type":"session_meta","payload":{"id":"S","cwd":"/repo"}}"#,
            r#"{"timestamp":"2026-04-21T10:00:30Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Always validate the tenant scope before issuing tokens."}]}}"#,
        );
        let (_d, path) = write_session(&body);
        let mut t = make_transcript(path);
        t.provider = Provider::Codex;
        let cands = harvest(&t, &HarvestParams::default()).unwrap();
        assert!(
            cands.is_empty(),
            "user-role messages must not produce candidates: {cands:?}"
        );
    }

    #[test]
    fn codex_turn_context_workdir_switch_resolves_relative_paths() {
        // After a `turn_context` event flips cwd to `/new`, a subsequent
        // `exec_command` with a relative path resolves against the new
        // workdir. The session-files-by-basename map then includes the
        // resolved path, and a sentence mentioning that path's basename
        // can pair with it.
        let cmd_in_new = "echo x > out.txt";
        let args = serde_json::json!({"cmd": cmd_in_new}).to_string();
        let body = format!(
            "{}\n{}\n{}\n{}",
            r#"{"timestamp":"2026-04-21T10:00:00Z","type":"session_meta","payload":{"id":"S","cwd":"/old"}}"#,
            r#"{"timestamp":"2026-04-21T10:00:10Z","type":"turn_context","payload":{"cwd":"/new"}}"#,
            r#"{"timestamp":"2026-04-21T10:00:30Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Never write to out.txt without locking first."}]}}"#,
            format!(
                r#"{{"timestamp":"2026-04-21T10:01:00Z","type":"response_item","payload":{{"type":"function_call","name":"exec_command","arguments":{}}}}}"#,
                serde_json::to_string(&args).unwrap()
            ),
        );
        let (_d, path) = write_session(&body);
        let mut t = make_transcript(path);
        t.provider = Provider::Codex;
        let cands = harvest(&t, &HarvestParams::default()).unwrap();
        assert_eq!(cands.len(), 1, "got: {cands:?}");
        let c = &cands[0];
        // The text mentions `out.txt`, which resolves under `/new`.
        // Sentence-level basename match wins → proximity 1.0.
        assert_eq!(
            c.target_hint.as_ref().map(|t| t.file.as_str()),
            Some("/new/out.txt"),
        );
        assert!((c.target_proximity - 1.0).abs() < 1e-6);
    }
}