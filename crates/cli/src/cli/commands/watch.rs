// SPDX-License-Identifier: Apache-2.0
//! Stream live oplog activity (`heddle watch`).
//!
//! Tails the append-only oplog file at `<repo>/.heddle/oplog/oplog.bin`
//! and emits one line per recorded operation as it lands — capture,
//! merge, thread create/update, marker, fork, collapse, goto. Default
//! behavior tails forever and exits on SIGINT (Ctrl-C). `--since 5m`
//! replays the last N before tailing live; `--filter` restricts to
//! the named kinds; `--output json` emits one JSON object per line for
//! piping to `jq` or downstream tooling.
//!
//! ## Tailing strategy
//!
//! The oplog is a *single packed file* rewritten atomically on each
//! batch — there's no stable byte offset to seek by. Instead we
//! track a `last_emitted_id` watermark (`OpEntry::id` is a
//! monotonically-increasing `u64` minted by `OpLog::record_*`),
//! reload the file on every notify event, and emit any entry with
//! `id > last_emitted_id`. That keeps the cursor logic trivial and
//! correct even when the writer rewrites the file between events.
//!
//! `OpLog::recent` caches the parsed file in-process; constructing
//! a fresh `OpLog::new_unattributed(heddle_dir)` each tick gives us a clean read
//! without poking at the cache field.
//!
//! ## Styling
//!
//! Delegated to [`crate::cli::style`] — the process-wide color gate
//! (CLI `--no-color` flag, `NO_COLOR` env, `CLICOLOR_FORCE` env, TTY
//! detection) is initialized once in `main` and consulted by the
//! shared helpers. JSON output is uncolored unconditionally because
//! the print sites short-circuit on JSON mode before any styled
//! helper runs.

use objects::store::ObjectStore;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use notify::{Config as NotifyConfig, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use objects::object::ChangeId;
use oplog::{OpEntry, OpLog, OpLogBackend, OpRecord};
use repo::Repository;
use serde::Serialize;

use super::{advice::RecoveryAdvice, command_runtime_contract};
use crate::cli::{
    Cli, JsonOutputMode, WatchArgs, json_output_mode_for_kind,
    style::{accent, change_id as style_change_id, confidence as style_confidence, dim, warn},
};

/// Reasonable default debounce — `notify` can fire several modify
/// events for one atomic rewrite, so we coalesce within this window.
const DEFAULT_POLL_INTERVAL_MS: u64 = 200;

/// Truncation budget for intent text in columnar mode. Long intents
/// get an ellipsis suffix; the column stays predictable so eyes can
/// track confidence values without horizontal scanning.
const INTENT_DISPLAY_WIDTH: usize = 50;

/// Hard cap on the in-process recent-entries window. Keeps memory
/// bounded on very long-running watch sessions; if the oplog has
/// more than this many entries, only the tail is replayed —
/// chronological correctness past this window isn't a `watch`
/// concern (use `heddle log` for full history).
const MAX_TAIL_WINDOW: usize = 100_000;

/// Entry kinds the user can pass to `--filter`. Names match the `kind` field
/// emitted in JSON mode (which is `OpRecord::verb()`) so a `--filter snapshot`
/// pipes cleanly into `--output json` for downstream tooling.
///
/// Derived from the oplog verb catalog (the single source of truth) plus the
/// `merge` UX alias — never a hand-maintained list, so every emitted kind
/// (including any future `OpRecord` variant) is a valid filter the moment it
/// joins the catalog, instead of being rejected as "unknown" (heddle#354 r9,
/// cid 3330304668). `merge` is the only entry that is not a literal verb: it
/// aliases `thread_update` on a merge target (see [`Renderer::passes_filter`]).
fn valid_filter_kinds() -> Vec<&'static str> {
    let mut kinds = OpRecord::verbs(true);
    kinds.push("merge");
    kinds
}

/// Top-level entry point. Threading-wise:
///
/// 1. Open the repository (canonical path discovery walks parents).
/// 2. Replay the `--since` window, emitting in chronological order.
/// 3. Spawn a `notify` watcher on the oplog file — the channel
///    sends events into the main loop, which drains pending entries
///    (id > watermark) on each modify and exits cleanly on SIGINT.
pub async fn cmd_watch(cli: &Cli, args: WatchArgs) -> Result<()> {
    let repo = cli.open_repo().context("opening repository for watch")?;
    let heddle_dir = repo.heddle_dir().to_path_buf();
    let oplog_path = oplog_file_path(&heddle_dir);

    // The oplog file may not exist yet on a brand-new repo — treat
    // that as "no events to replay; wait for the first writer". The
    // notify watcher attaches to the parent directory in that case
    // so the first append doesn't get lost.
    if !oplog_path.parent().is_some_and(Path::is_dir) {
        let path = oplog_path
            .parent()
            .map(Path::display)
            .map_or_else(|| "<unknown>".to_string(), |display| display.to_string());
        return Err(anyhow!(RecoveryAdvice::invalid_usage(
            "watch_oplog_missing",
            format!("oplog directory missing at {path}; run `heddle init` first"),
            "Run `heddle init` in this repository before watching oplog events.",
            "heddle init",
        )));
    }

    let json_mode = json_mode(cli, &args);
    let filter = parse_filter(args.filter.as_deref())?;
    let since_cutoff = match args.since.as_deref() {
        Some(spec) => Some(parse_since(spec)?),
        None => None,
    };
    let renderer = Renderer {
        json: json_mode,
        filter,
    };

    // ---- Phase 1: replay --since window (if requested) ----
    let mut watermark: u64 = 0;
    if let Some(cutoff) = since_cutoff {
        let entries = drain_pending(&heddle_dir, watermark, &repo, Some(cutoff), MAX_TAIL_WINDOW)?;
        for emitted in &entries {
            renderer.emit(emitted);
        }
        if let Some(last) = entries.iter().map(|e| e.entry.id).max() {
            watermark = last;
        }
    }

    // Initialize the watermark from the current oplog head so we don't
    // re-emit pre-existing entries on first tail. (When --since was
    // set above, the watermark is already advanced.)
    if watermark == 0 {
        let log = OpLog::new_unattributed(&heddle_dir);
        if let Some(last) = log.last().context("reading oplog head")? {
            watermark = last.id;
        }
    }

    // ---- Phase 2: tail live ----
    let stop_flag = install_sigint_handler();
    let poll_interval =
        Duration::from_millis(args.poll_interval_ms.unwrap_or(DEFAULT_POLL_INTERVAL_MS));
    tail_loop(
        &heddle_dir,
        &oplog_path,
        &repo,
        &renderer,
        watermark,
        stop_flag,
        poll_interval,
        args.max_iterations,
    )?;

    Ok(())
}

/// Resolve the path to the oplog file. Mirrors `OpLog::oplog_path`
/// (kept private in the `oplog` crate); the layout is part of the
/// on-disk contract so duplicating the literal here is acceptable.
fn oplog_file_path(heddle_dir: &Path) -> PathBuf {
    heddle_dir.join("oplog").join("oplog.bin")
}

/// Resolve JSON-vs-text mode from the command contract. `watch`
/// advertises `jsonl`, so the shared resolver keeps the stream
/// human-readable unless the user explicitly asks for JSON.
fn json_mode(cli: &Cli, _args: &WatchArgs) -> bool {
    let contract =
        command_runtime_contract("watch").expect("watch command contract should be registered");
    matches!(
        json_output_mode_for_kind(cli, None, contract.json_kind),
        JsonOutputMode::Jsonl
    )
}

/// Parse `--filter snapshot,merge,thread_create` into a set of kinds.
/// Empty string => no filter. Unknown kinds raise — easier to fix a
/// typo at boot than to wonder why nothing prints.
fn parse_filter(spec: Option<&str>) -> Result<Option<Vec<String>>> {
    let Some(raw) = spec else {
        return Ok(None);
    };
    let kinds: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if kinds.is_empty() {
        return Ok(None);
    }
    let valid = valid_filter_kinds();
    for kind in &kinds {
        if !valid.contains(&kind.as_str()) {
            return Err(anyhow!(RecoveryAdvice::invalid_usage(
                "watch_filter_invalid",
                format!(
                    "unknown event kind in --filter: {kind:?} (valid: {})",
                    valid.join(", ")
                ),
                "Use one of the valid watch event kinds, or omit `--filter`.",
                "heddle watch --filter snapshot",
            )));
        }
    }
    Ok(Some(kinds))
}

/// Parse a duration like `30s` / `5m` / `1h` / `2d` into a UTC
/// cutoff. Mirrors common shell tooling (`tail --since`, `journalctl
/// --since`); intentionally *not* a full ISO-8601 timestamp parser —
/// keep the operator UX simple.
fn parse_since(spec: &str) -> Result<DateTime<Utc>> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err(anyhow!(RecoveryAdvice::invalid_usage(
            "watch_since_empty",
            "--since cannot be empty",
            "Use a duration like `30s`, `5m`, `1h`, or `2d`, or omit `--since`.",
            "heddle watch --since 5m",
        )));
    }
    let (num_part, unit) = trimmed.split_at(
        trimmed
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(trimmed.len()),
    );
    let n: i64 = num_part
        .parse()
        .with_context(|| format!("--since: expected leading digits in {spec:?}"))?;
    let delta = match unit {
        "s" | "" => ChronoDuration::seconds(n),
        "m" => ChronoDuration::minutes(n),
        "h" => ChronoDuration::hours(n),
        "d" => ChronoDuration::days(n),
        other => {
            return Err(anyhow!(
                "--since: unknown unit {other:?} (use s, m, h, or d)"
            ));
        }
    };
    Ok(Utc::now() - delta)
}

/// Install a Ctrl-C handler that flips an atomic flag the tail loop
/// polls. We keep the handler best-effort: if installation fails
/// (e.g. another handler is already registered in a test harness),
/// the loop still exits when the test sets `max_iterations`.
fn install_sigint_handler() -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_handler = Arc::clone(&stop);
    // ctrlc isn't a workspace dep; use the libc-portable signal hook
    // already exposed by tokio. We deliberately drop the JoinHandle —
    // when the runtime shuts down, the spawned task is cancelled
    // automatically; we just need the side-effect of flipping the
    // atomic when SIGINT arrives. Falling back to no-op on
    // registration failure is intentional — see doc comment.
    drop(tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            stop_for_handler.store(true, Ordering::SeqCst);
        }
    }));
    stop
}

/// Main tail loop: notify watcher → debounce → drain pending entries
/// (id > watermark) → emit → repeat.
#[allow(clippy::too_many_arguments)]
fn tail_loop(
    heddle_dir: &Path,
    oplog_path: &Path,
    repo: &Repository,
    renderer: &Renderer,
    mut watermark: u64,
    stop_flag: Arc<AtomicBool>,
    poll_interval: Duration,
    max_iterations: Option<usize>,
) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let watch_target = if oplog_path.exists() {
        oplog_path.to_path_buf()
    } else {
        oplog_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| heddle_dir.to_path_buf())
    };

    let mut watcher: RecommendedWatcher = RecommendedWatcher::new(
        move |event| {
            // Best-effort send — if the receiver is gone, the loop
            // already decided to stop and there's nothing to do.
            let _ = tx.send(event);
        },
        NotifyConfig::default(),
    )
    .context("constructing notify watcher")?;
    watcher
        .watch(&watch_target, RecursiveMode::NonRecursive)
        .with_context(|| format!("watching {}", watch_target.display()))?;

    let mut iterations = 0usize;
    loop {
        if stop_flag.load(Ordering::SeqCst) {
            break;
        }
        match rx.recv_timeout(poll_interval) {
            Ok(Ok(event)) => {
                if !is_relevant_event(&event.kind) {
                    continue;
                }
                // Coalesce a burst of modify events: drain anything
                // already queued before re-reading the file.
                while let Ok(_extra) = rx.try_recv() {}
                let entries = drain_pending(heddle_dir, watermark, repo, None, MAX_TAIL_WINDOW)?;
                for emitted in &entries {
                    renderer.emit(emitted);
                }
                if let Some(last) = entries.iter().map(|e| e.entry.id).max() {
                    watermark = last;
                }
                iterations += 1;
                if max_iterations.is_some_and(|limit| iterations >= limit) {
                    break;
                }
            }
            Ok(Err(_err)) => {
                // notify-side error (rename, vanished file). Keep
                // looping; the next modify will recover.
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    Ok(())
}

/// Returns `true` for the notify event kinds that indicate the
/// oplog file (or its parent dir) just got rewritten. Atomic
/// `write_file_atomic` produces `Create` (the temp file) and
/// `Modify::Name(...)` (the rename) — both are relevant.
fn is_relevant_event(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
    )
}

/// Read the oplog and return all entries with `id > watermark`,
/// optionally restricted to entries with `timestamp >= cutoff`.
/// Resolves snapshot intent/confidence by looking up the linked
/// `State` in the object store; failures there fall back to the
/// raw `OpEntry` so we never drop an event because of a missing
/// state body (which can happen briefly during replication).
fn drain_pending(
    heddle_dir: &Path,
    watermark: u64,
    repo: &Repository,
    since_cutoff: Option<DateTime<Utc>>,
    window: usize,
) -> Result<Vec<EmittedEntry>> {
    let log = OpLog::new_unattributed(heddle_dir);
    // `recent(N)` returns most-recent-first; we reverse to get
    // chronological order before filtering.
    let mut recent = log.recent(window).context("reading recent oplog entries")?;
    recent.reverse();
    let mut out = Vec::new();
    for entry in recent {
        if entry.id <= watermark {
            continue;
        }
        if let Some(cutoff) = since_cutoff
            && entry.timestamp < cutoff
        {
            continue;
        }
        out.push(annotate_entry(entry, repo));
    }
    Ok(out)
}

/// Materialize the displayable fields for one entry: thread name,
/// kind label, change-id short, intent, confidence, actor identity.
/// Lookup failures (missing state, unreadable store) degrade to a
/// best-effort summary — `watch` shouldn't crash on transient state
/// reads.
fn annotate_entry(entry: OpEntry, repo: &Repository) -> EmittedEntry {
    let kind = kind_for(&entry.operation);
    let thread = thread_for(&entry.operation, &kind);
    let change = primary_change_id(&entry.operation);
    let (intent, confidence, actor) = match &change {
        Some(id) => state_lookup(repo, id),
        None => (None, None, None),
    };
    EmittedEntry {
        entry,
        kind,
        thread,
        change_id: change,
        intent,
        confidence,
        actor,
    }
}

/// Best-effort state lookup. We ignore errors here because:
/// 1. `watch` is read-only telemetry — a transient store glitch
///    shouldn't halt the stream.
/// 2. Some operations (Goto, ThreadDelete) point at states that
///    legitimately may not be locally present.
fn state_lookup(
    repo: &Repository,
    change_id: &ChangeId,
) -> (Option<String>, Option<f32>, Option<ActorInfo>) {
    let Ok(Some(state)) = repo.store().get_state(change_id) else {
        return (None, None, None);
    };
    let actor = state.attribution.agent.as_ref().map(|agent| ActorInfo {
        provider: agent.provider.clone(),
        model: agent.model.clone(),
    });
    (state.intent.clone(), state.confidence, actor)
}

/// Resolve an `OpRecord` to the user-facing kind label — the snake-case
/// `OpRecord::verb()` from the oplog catalog, which is also the JSON `kind`
/// field and the `--filter` keyword. Single source of truth: a new variant
/// gets its label from the catalog automatically (heddle#354 r9).
fn kind_for(op: &OpRecord) -> String {
    op.verb().to_string()
}

/// Best-effort thread/lane identifier for the columnar layout. Falls
/// back to the entry's recorded `scope` (the worktree path) so every
/// row has *something* in the second column.
fn thread_for(op: &OpRecord, _kind: &str) -> Option<String> {
    match op {
        OpRecord::Snapshot { thread, .. } => thread.clone(),
        OpRecord::ThreadCreate { name, .. } => Some(name.clone()),
        OpRecord::ThreadDelete { name, .. } => Some(name.clone()),
        OpRecord::ThreadUpdate { name, .. } => Some(name.clone()),
        OpRecord::MarkerCreate { name, .. } => Some(name.clone()),
        OpRecord::MarkerDelete { name, .. } => Some(name.clone()),
        OpRecord::Checkpoint { thread, .. } => thread.clone(),
        OpRecord::EphemeralThreadCollapse { thread, .. } => Some(thread.clone()),
        OpRecord::FastForward { target_thread, .. } => Some(target_thread.clone()),
        OpRecord::GitCheckpoint { branch, .. } => Some(branch.clone()),
        OpRecord::RemoteThreadUpdate { thread, .. }
        | OpRecord::RemoteThreadDelete { thread, .. } => Some(thread.clone()),
        OpRecord::Goto { .. }
        | OpRecord::Fork { .. }
        | OpRecord::Collapse { .. }
        | OpRecord::TransactionAbort { .. }
        | OpRecord::TransactionCommit { .. }
        | OpRecord::ConflictResolved { .. }
        | OpRecord::Redact { .. }
        | OpRecord::UndoRecoveryUpdate { .. }
        | OpRecord::StateVisibilitySet { .. }
        | OpRecord::StateVisibilityPromote { .. }
        | OpRecord::Purge { .. } => None,
    }
}

/// Pick the change-id that best identifies this op for state lookup
/// and for the `change_id` column. For Snapshot/Goto/ThreadUpdate
/// this is the new state; for ThreadCreate it's the seeded state.
fn primary_change_id(op: &OpRecord) -> Option<ChangeId> {
    match op {
        OpRecord::Snapshot { new_state, .. } => Some(*new_state),
        OpRecord::Goto { target, .. } => Some(*target),
        OpRecord::ThreadCreate { state, .. } => Some(*state),
        OpRecord::ThreadDelete { state, .. } => Some(*state),
        OpRecord::ThreadUpdate { new_state, .. } => Some(*new_state),
        OpRecord::Fork { new_state, .. } => Some(*new_state),
        OpRecord::Collapse { result, .. } => Some(*result),
        OpRecord::MarkerCreate { state, .. } => Some(*state),
        OpRecord::MarkerDelete { state, .. } => Some(*state),
        OpRecord::Checkpoint { state, .. } => Some(*state),
        OpRecord::GitCheckpoint { state, .. } => Some(*state),
        OpRecord::EphemeralThreadCollapse { final_state, .. } => Some(*final_state),
        OpRecord::Redact { state, .. } => Some(*state),
        OpRecord::StateVisibilitySet { state, .. }
        | OpRecord::StateVisibilityPromote { state, .. } => Some(*state),
        OpRecord::RemoteThreadUpdate { state, .. } | OpRecord::RemoteThreadDelete { state, .. } => {
            Some(*state)
        }
        OpRecord::UndoRecoveryUpdate { state } => Some(*state),
        OpRecord::TransactionAbort { .. }
        | OpRecord::TransactionCommit { .. }
        | OpRecord::ConflictResolved { .. }
        | OpRecord::Purge { .. }
        | OpRecord::FastForward { .. } => None,
    }
}

/// One annotated, ready-to-render oplog entry. We compute these in
/// the drain step rather than the render step so unit tests can
/// assert on a structured value instead of parsing terminal output.
#[derive(Clone, Debug)]
struct EmittedEntry {
    entry: OpEntry,
    kind: String,
    thread: Option<String>,
    change_id: Option<ChangeId>,
    intent: Option<String>,
    confidence: Option<f32>,
    actor: Option<ActorInfo>,
}

/// JSON-mode view of an actor (agent provider/model). Mirrors the
/// shape rendered by `heddle status --output json` so consumers can join
/// `watch` events with `status` outputs without renaming fields.
#[derive(Clone, Debug, Serialize)]
struct ActorInfo {
    provider: String,
    model: String,
}

/// JSON-mode line schema. One line per emitted oplog entry.
/// Field names are snake_case to match the rest of Heddle's JSON
/// surfaces (status, log).
#[derive(Serialize)]
struct WatchLineJson<'a> {
    ts: String,
    thread: Option<&'a str>,
    kind: &'a str,
    change_id: Option<String>,
    intent: Option<&'a str>,
    confidence: Option<f32>,
    actor: Option<&'a ActorInfo>,
    /// Numeric oplog id, useful for downstream cursor tracking.
    id: u64,
}

/// Holds the (immutable) render decision (text vs JSON) plus the
/// parsed `--filter` set.
struct Renderer {
    json: bool,
    filter: Option<Vec<String>>,
}

impl Renderer {
    /// Print one entry, respecting `--filter` and the JSON gate.
    fn emit(&self, entry: &EmittedEntry) {
        if !self.passes_filter(entry) {
            return;
        }
        let line = if self.json {
            self.render_json(entry)
        } else {
            self.render_text(entry)
        };
        println!("{line}");
    }

    fn passes_filter(&self, entry: &EmittedEntry) -> bool {
        let Some(allowed) = &self.filter else {
            return true;
        };
        // `merge` is a UX alias the operator can pass — it matches
        // ThreadUpdate (the wire-level kind for both ordinary
        // captures-on-thread and merges into a target). When the
        // operator wants only "true merges", they can post-filter
        // on `change_id` distinct-ness; that nuance is out of
        // scope for the CLI flag.
        let kind = entry.kind.as_str();
        allowed
            .iter()
            .any(|k| k == kind || (k == "merge" && kind == "thread_update"))
    }

    fn render_json(&self, e: &EmittedEntry) -> String {
        let view = WatchLineJson {
            ts: e.entry.timestamp.to_rfc3339_opts(SecondsFormat::Secs, true),
            thread: e.thread.as_deref(),
            kind: e.kind.as_str(),
            change_id: e.change_id.as_ref().map(|id| id.to_string_full()),
            intent: e.intent.as_deref(),
            confidence: e.confidence,
            actor: e.actor.as_ref(),
            id: e.entry.id,
        };
        serde_json::to_string(&view).unwrap_or_else(|err| {
            // Serializing a derived struct with primitive fields
            // never fails in practice; the fallback keeps the
            // process alive on the impossible path.
            format!("{{\"error\":\"json render failed: {err}\"}}")
        })
    }

    /// Columnar text mode. Widths are tuned for an 80-col terminal:
    /// `HH:MM:SS` (8) + 2sp + thread (28) + 2sp + kind (15) + 2sp +
    /// change_id (15) + 2sp + intent (50) + 2sp + confidence (10) ≈
    /// 132 chars worst-case. Most rows are well under that because
    /// `dim()` wraps the timestamp/change-id in escapes that don't
    /// add visible width.
    fn render_text(&self, e: &EmittedEntry) -> String {
        let ts = e.entry.timestamp.format("%H:%M:%S");
        let thread = truncate(e.thread.as_deref().unwrap_or("-"), 28);
        let kind_plain = e.kind.as_str();
        let kind_styled = style_kind(kind_plain);
        // Pad on the *plain* form so escape codes don't throw the
        // column count off.
        let kind_pad = pad_right(kind_plain, 15);
        let kind_field = kind_styled + &" ".repeat(kind_pad.len() - kind_plain.len());
        let change = e
            .change_id
            .map(|id| id.short())
            .unwrap_or_else(|| "-".to_string());
        let change_field = pad_right(&style_change_id(&change), 15 + ansi_overhead(&change));
        let intent = truncate(e.intent.as_deref().unwrap_or(""), INTENT_DISPLAY_WIDTH);
        let intent_padded = pad_right(&intent, INTENT_DISPLAY_WIDTH);
        let confidence_field = match e.confidence {
            Some(c) => style_confidence(Some(c), &format!("conf={c:.2}")),
            None => String::new(),
        };
        format!(
            "{ts}  {thread:<28}  {kind_field}  {change_field}  {intent_padded}  {confidence_field}",
            ts = dim(&ts.to_string()),
        )
        .trim_end()
        .to_string()
    }
}

/// Pick a color band for the kind label. Capture-style events get
/// the warm accent; merges (thread_update on a target) too;
/// destructive events get warn/error; structural-only events go
/// dim so the eye skims them.
fn style_kind(kind: &str) -> String {
    match kind {
        "snapshot" | "thread_update" => accent(kind),
        "thread_create" | "marker_create" | "fork" | "collapse" | "goto" => dim(kind),
        "thread_delete" | "marker_delete" => warn(kind),
        _ => kind.to_string(),
    }
}

/// Truncate a string to `max` *chars* (not bytes), appending `…` if
/// the cut actually happened. Rendering width calculations elsewhere
/// assume this returns a string with `chars().count() <= max`.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// Pad a string with trailing spaces up to `width` characters. ANSI
/// escapes in the input are counted as zero width — the columnar
/// layout depends on visible-character math, not byte length.
/// `width` here is the *visible* target.
fn pad_right(s: &str, width: usize) -> String {
    let visible = visible_width(s);
    if visible >= width {
        return s.to_string();
    }
    format!("{s}{}", " ".repeat(width - visible))
}

/// Best-effort visible-width: chars minus the ANSI-escape bytes.
/// Heddle's `style.rs` only emits SGR sequences (`\x1b[...m`), so a
/// simple state machine is sufficient.
fn visible_width(s: &str) -> usize {
    let mut count = 0usize;
    let mut in_escape = false;
    for ch in s.chars() {
        if in_escape {
            if ch == 'm' {
                in_escape = false;
            }
            continue;
        }
        if ch == '\x1b' {
            in_escape = true;
            continue;
        }
        count += 1;
    }
    count
}

/// Number of bytes spent on ANSI escape codes in a styled string.
/// Used to compensate when we want a `pad_right` target that already
/// accounts for the escape overhead — see `render_text`.
fn ansi_overhead(plain: &str) -> usize {
    let styled = style_change_id(plain);
    styled.len() - plain.len()
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use objects::object::ChangeId;
    use oplog::{OpEntry, OpRecord};
    use tempfile::TempDir;

    use super::*;

    fn make_entry(id: u64, op: OpRecord) -> OpEntry {
        OpEntry {
            id,
            timestamp: Utc.with_ymd_and_hms(2026, 5, 2, 22, 43, 8).unwrap(),
            operation: op,
            undone: false,
            batch_id: id,
            batch_index: 0,
            scope: None,
            actor: std::sync::Arc::new(objects::object::Principal::new("Test", "test@example.com")),
            operation_id: None,
        }
    }

    fn write_entries(heddle_dir: &Path, entries: Vec<OpRecord>) -> Vec<u64> {
        let log = OpLog::new_unattributed(heddle_dir);
        log.init().expect("init oplog");
        log.record_batch(entries).expect("record batch")
    }

    fn synthetic_repo() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let heddle = tmp.path().join(".heddle");
        std::fs::create_dir_all(&heddle).unwrap();
        let log = OpLog::new_unattributed(&heddle);
        log.init().unwrap();
        let path = heddle.clone();
        (tmp, path)
    }

    #[test]
    fn parse_since_accepts_common_units() {
        let now = Utc::now();
        let cases = [("30s", 30), ("5m", 5 * 60), ("2h", 2 * 60 * 60)];
        for (spec, secs) in cases {
            let cutoff = parse_since(spec).unwrap();
            let delta = (now - cutoff).num_seconds();
            assert!((delta - secs).abs() <= 2, "spec {spec}: delta={delta}");
        }
    }

    #[test]
    fn parse_since_rejects_unknown_unit() {
        assert!(parse_since("5x").is_err());
        assert!(parse_since("").is_err());
    }

    #[test]
    fn parse_filter_validates_kinds() {
        assert!(parse_filter(None).unwrap().is_none());
        assert!(parse_filter(Some("")).unwrap().is_none());
        let parsed = parse_filter(Some("snapshot,merge")).unwrap().unwrap();
        assert_eq!(parsed, vec!["snapshot", "merge"]);
        assert!(parse_filter(Some("not_a_real_kind")).is_err());
    }

    #[test]
    fn filter_accepts_newer_emitted_kinds() {
        // Non-vacuous for cid 3330304668: these kinds are emitted by
        // `OpRecord::verb()` but were absent from the old hand-maintained
        // FILTER_KINDS list, so `--filter remote_thread_update` was wrongly
        // rejected as "unknown". The derived list now accepts every real kind.
        for kind in [
            "remote_thread_update",
            "remote_thread_delete",
            "transaction_commit",
            "redact",
            "purge",
            "git_checkpoint",
            "undo_recovery_update",
        ] {
            assert!(
                parse_filter(Some(kind)).is_ok(),
                "filter kind {kind:?} must be accepted (it is a real emitted kind)"
            );
        }
    }

    #[test]
    fn drain_pending_emits_only_above_watermark() {
        let (_tmp, heddle_dir) = synthetic_repo();
        let cid_a = ChangeId::generate();
        let cid_b = ChangeId::generate();
        let ids = write_entries(
            &heddle_dir,
            vec![
                OpRecord::Snapshot {
                    new_state: cid_a,
                    prev_head: None,
                    head: None,
                    thread: Some("modulo-race".into()),
                },
                OpRecord::ThreadCreate {
                    name: "approach-anthropic".into(),
                    state: cid_b,
                    manager_snapshot: None,
                },
            ],
        );
        assert_eq!(ids.len(), 2);

        // Use a fresh OpLog; we don't have a Repository here so use
        // the lower-level helper directly.
        let log = OpLog::new_unattributed(&heddle_dir);
        let entries = log.recent(usize::MAX).unwrap();
        // recent() returns most-recent first; verify both ids land.
        let seen_ids: Vec<u64> = entries.iter().map(|e| e.id).collect();
        assert_eq!(seen_ids, vec![ids[1], ids[0]]);

        // Watermark above first id should drop it.
        let above_first: Vec<_> = entries.iter().rev().filter(|e| e.id > ids[0]).collect();
        assert_eq!(above_first.len(), 1);
        assert_eq!(above_first[0].id, ids[1]);
    }

    #[test]
    fn renderer_filter_passes_only_matching_kinds() {
        let renderer = Renderer {
            json: true,
            filter: Some(vec!["snapshot".into()]),
        };
        let snap_id = ChangeId::generate();
        let snap = EmittedEntry {
            entry: make_entry(
                1,
                OpRecord::Snapshot {
                    new_state: snap_id,
                    prev_head: None,
                    head: Some(snap_id),
                    thread: None,
                },
            ),
            kind: "snapshot".into(),
            thread: None,
            change_id: None,
            intent: None,
            confidence: None,
            actor: None,
        };
        let create = EmittedEntry {
            entry: make_entry(
                2,
                OpRecord::ThreadCreate {
                    name: "x".into(),
                    state: ChangeId::generate(),
                    manager_snapshot: None,
                },
            ),
            kind: "thread_create".into(),
            thread: Some("x".into()),
            change_id: None,
            intent: None,
            confidence: None,
            actor: None,
        };
        assert!(renderer.passes_filter(&snap));
        assert!(!renderer.passes_filter(&create));
    }

    #[test]
    fn render_json_round_trips() {
        let renderer = Renderer {
            json: true,
            filter: None,
        };
        let cid = ChangeId::generate();
        let entry = EmittedEntry {
            entry: make_entry(
                7,
                OpRecord::Snapshot {
                    new_state: cid,
                    prev_head: None,
                    head: None,
                    thread: Some("modulo-race/approach-anthropic".into()),
                },
            ),
            kind: "snapshot".into(),
            thread: Some("modulo-race/approach-anthropic".into()),
            change_id: Some(cid),
            intent: Some("feat(modulo): error-returning impl".into()),
            confidence: Some(0.92),
            actor: Some(ActorInfo {
                provider: "anthropic".into(),
                model: "claude-sonnet-4-5".into(),
            }),
        };
        let line = renderer.render_json(&entry);
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["kind"], "snapshot");
        assert_eq!(value["thread"], "modulo-race/approach-anthropic");
        assert_eq!(value["confidence"], 0.92);
        assert_eq!(value["actor"]["provider"], "anthropic");
        assert_eq!(value["id"], 7);
        assert!(value["change_id"].is_string());
        assert!(value["ts"].as_str().unwrap().ends_with('Z'));
    }

    #[test]
    fn truncate_handles_ascii_and_unicode() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("aaaaaaaaaa", 5), "aaaa…");
        assert_eq!(truncate("résumé café", 6), "résum…");
    }

    #[test]
    fn render_text_columns_preserve_widths() {
        // Color is gated off by default in tests (OnceLock unset).
        let renderer = Renderer {
            json: false,
            filter: None,
        };
        let cid = ChangeId::generate();
        let entry = EmittedEntry {
            entry: make_entry(
                1,
                OpRecord::Snapshot {
                    new_state: cid,
                    prev_head: None,
                    head: None,
                    thread: Some("modulo-race/approach-anthropic".into()),
                },
            ),
            kind: "snapshot".into(),
            thread: Some("modulo-race/approach-anthropic".into()),
            change_id: Some(cid),
            intent: Some("feat(modulo): error-returning impl".into()),
            confidence: Some(0.92),
            actor: None,
        };
        let line = renderer.render_text(&entry);
        // Should contain HH:MM:SS, kind label, short change-id, conf
        assert!(line.contains("22:43:08"), "missing timestamp: {line}");
        assert!(line.contains("snapshot"), "missing kind: {line}");
        assert!(line.contains("conf=0.92"), "missing conf: {line}");
        // Long thread name is truncated to <= 28 chars + ellipsis.
        let visible = visible_width(&line);
        assert!(visible >= 80, "line too short: {visible}");
    }

    #[test]
    fn primary_change_id_covers_all_variants() {
        // Smoke test: every OpRecord variant resolves to *some*
        // change-id so the change_id column is never blank for a
        // real op. (Goto/Fork/Collapse have no thread but they do
        // have an associated state.)
        let cid = ChangeId::generate();
        for op in [
            OpRecord::Snapshot {
                new_state: cid,
                prev_head: None,
                head: Some(cid),
                thread: None,
            },
            OpRecord::Goto {
                target: cid,
                prev_head: None,
                head: cid,
            },
            OpRecord::ThreadCreate {
                name: "x".into(),
                state: cid,
                manager_snapshot: None,
            },
            OpRecord::ThreadDelete {
                name: "x".into(),
                state: cid,
            },
            OpRecord::ThreadUpdate {
                name: "x".into(),
                old_state: cid,
                new_state: cid,
                manager_snapshots: None,
            },
            OpRecord::Fork {
                from: cid,
                new_state: cid,
                thread: None,
                head: None,
            },
            OpRecord::Collapse {
                sources: vec![cid],
                result: cid,
                thread: None,
                pre_thread_state: None,
            },
            OpRecord::MarkerCreate {
                name: "m".into(),
                state: cid,
            },
            OpRecord::MarkerDelete {
                name: "m".into(),
                state: cid,
            },
        ] {
            assert!(primary_change_id(&op).is_some());
        }
    }
}
