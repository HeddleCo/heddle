// SPDX-License-Identifier: Apache-2.0
//! `heddle attempt N -- <cmd>` — best-of-N parallel-try.
//!
//! Implements item 3.2 of the heddle 6→8 plan. Conceptually: `heddle
//! try` × N, with a comparison + ranking pass at the end.
//!
//! ### Why this is its own verb
//!
//! `heddle try` is "run-once-and-commit-if-it-works". Real agent
//! workflows often need "spawn N candidate solutions, evaluate them,
//! pick the winner". Today that means manually spinning up worktrees,
//! coordinating shell processes, and parsing exit codes by hand.
//! `heddle attempt` makes that a primitive.
//!
//! ### Cargo `target/` multiplication — the load-bearing default
//!
//! N parallel `cargo build` invocations against this codebase's own
//! workspace consume tens of GB of disk if each thread keeps its own
//! `target/`. To avoid that footgun, `--shared-target` defaults to ON
//! whenever the workspace root contains a `Cargo.toml`. Pass
//! `--no-shared-target` to opt out (useful when you're testing the
//! build cache itself, or running a non-cargo workload that wouldn't
//! benefit).
//!
//! ### Working-tree invariant
//!
//! Same as `heddle try`: the parent thread's working tree must end in
//! exactly the state it started, regardless of how the attempts
//! resolve. Each attempt runs inside its own ephemeral checkout via
//! `Command::current_dir(&thread_path)`; the parent is never touched.
//! `heddle attempt` does not auto-merge — the user picks the winner
//! by inspecting the table and running `heddle merge <thread>` (the
//! recommended verb is included in the output for direct copy-paste).

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::Instant,
};

use anyhow::{Result, anyhow};
use repo::Repository;
use serde::Serialize;

use super::{
    diff::compute_state_diff,
    snapshot::{SnapshotAgentOverrides, create_snapshot},
    thread::start_thread,
    thread_cmd::drop_thread_silent,
    try_cmd::thread_name_in_use,
    worktree_cmd::shared_target as shared_target_helpers,
};
use crate::{
    cli::{AttemptArgs, Cli, ThreadStartArgs, WorkspaceModeArg, should_output_json, style},
    config::UserConfig,
};

/// Hard ceiling on N — protects shared CI machines from a fork-bomb
/// invocation. Eight cargo workloads in flight already saturate most
/// developer hardware; ten is generous and round.
const MAX_ATTEMPTS: u32 = 10;

/// Status label for an individual attempt.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum AttemptStatus {
    /// Primary cmd succeeded (and `--evaluate`, if set, also succeeded).
    Succeeded,
    /// Primary cmd succeeded but the `--evaluate` cmd failed.
    EvaluateFailed,
    /// Primary cmd exited non-zero.
    Failed,
    /// Primary cmd never started (program-not-found, etc.). Always
    /// drops; never makes it to the ranking.
    SpawnError,
}

/// Per-attempt outcome accumulated during the parallel run. Owned by
/// the spawned thread; collected via a shared `Mutex<Vec<_>>`. Carries
/// every signal we'll need at ranking time.
#[derive(Debug, Clone, Serialize)]
struct AttemptResult {
    /// Index assigned at spawn time (1-based for display, 0-based in
    /// the result vector). Used for stable ordering when ranking is
    /// otherwise undecided.
    index: usize,
    thread: String,
    status: AttemptStatus,

    /// Primary cmd exit code. `None` when the process was killed by
    /// a signal or never started.
    primary_exit_code: Option<i32>,
    /// Wall-clock duration of the primary cmd in seconds. Used as the
    /// quaternary sort key.
    primary_duration_secs: f64,

    /// `--evaluate` cmd exit code. `None` when `--evaluate` wasn't
    /// passed, or when the primary cmd failed (skip evaluate). `Some`
    /// when evaluate ran.
    evaluate_exit_code: Option<i32>,
    /// `--evaluate` wall-clock duration. `None` when not run.
    evaluate_duration_secs: Option<f64>,

    /// Captured state on the attempt's thread. `None` on the failure
    /// path or when capture itself returned an error.
    captured_state: Option<String>,
    /// Number of files in the parent ↔ thread-tip diff. Tertiary sort
    /// key — smaller diffs win when everything else is equal.
    diff_files: Option<usize>,

    /// Set when the attempt's thread was dropped (failure cleanup).
    thread_dropped: bool,

    /// Free-form note for surface-level errors that don't fit the
    /// exit-code shape (e.g. spawn errors, capture failures).
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

/// JSON output for `heddle attempt`. Mirrors the shape of `TryOutput`
/// but with a `Vec<AttemptResult>` and a `recommended` thread name.
#[derive(Debug, Serialize)]
struct AttemptOutput {
    status: &'static str,
    action: &'static str,
    message: String,

    /// User-supplied `<cmd>` joined for display.
    command: String,
    /// User-supplied `--evaluate` cmd, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    evaluate: Option<String>,

    /// Number of attempts that were spawned (after N validation).
    attempts_total: usize,
    /// How many primary cmds exited 0.
    attempts_succeeded: usize,
    /// How many ephemeral threads were dropped during cleanup.
    attempts_dropped: usize,

    /// Ranked attempts, best first.
    attempts: Vec<AttemptResult>,

    /// Thread name of the recommended winner. `None` when no attempt
    /// succeeded; the message will explain why.
    #[serde(skip_serializing_if = "Option::is_none")]
    recommended: Option<String>,

    /// `heddle merge <recommended> --with-diff` hint, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    next_action: Option<String>,
}

pub fn cmd_attempt(cli: &Cli, args: AttemptArgs) -> Result<()> {
    if args.command.is_empty() {
        return Err(anyhow!("Usage: heddle attempt <N> -- <cmd...>"));
    }
    if args.n == 0 {
        return Err(anyhow!("N must be at least 1 (was 0)"));
    }
    if args.n > MAX_ATTEMPTS {
        return Err(anyhow!(
            "heddle attempt is capped at {} parallel attempts (you asked for {}). \
            For higher fan-out, run multiple `heddle attempt` invocations.",
            MAX_ATTEMPTS,
            args.n
        ));
    }

    let repo_root_arg = cli
        .repo
        .as_ref()
        .cloned()
        .unwrap_or(std::env::current_dir()?);
    let repo = Repository::open(&repo_root_arg)?;

    // Snapshot the parent's HEAD up front so we can verify the
    // worktree-invariant after all attempts resolve. `heddle attempt`
    // never advances the parent's HEAD — the user opts in by running
    // `heddle merge <winner>` afterwards.
    let parent_head_before = repo.head()?.map(|id| id.to_string_full());

    // Resolve the shared-target default. If the user passed
    // `--no-shared-target`, that wins. If they passed `--shared-target`,
    // that wins. Otherwise the default is ON for Rust workspaces and
    // OFF elsewhere — see the module docs for why.
    let shared_target = if args.no_shared_target {
        false
    } else if args.shared_target {
        true
    } else {
        shared_target_helpers::workspace_root_is_rust(&repo)
    };

    let workspace = match args.workspace {
        WorkspaceModeArg::Auto => WorkspaceModeArg::Heavy,
        other => other,
    };

    let prefix = args
        .name_prefix
        .clone()
        .unwrap_or_else(|| default_attempt_prefix(&args.command));

    // Preflight ALL N synthesized names before spawning any attempts.
    // `cmd_attempt` deterministically names threads `<prefix>-1`,
    // `<prefix>-2`, …, `<prefix>-N` and passes them straight into
    // `start_thread`, which is create-or-resume — so if a name
    // collides with a thread that already exists (by manager record OR
    // by ref), `start_thread` would attach to it and the failure-path
    // `drop_thread_silent` would later abandon it. That's user-data
    // destructive when `--name-prefix` overlaps prior threads.
    //
    // Refuse all-or-nothing: don't spawn 1..K then fail on K+1. Only
    // user-supplied prefixes are at risk; auto-generated prefixes
    // embed a hash and won't collide, but checking unconditionally
    // costs nothing and keeps the contract simple.
    for i in 1..=args.n {
        let name = format!("{prefix}-{i}");
        if thread_name_in_use(&repo, &name)? {
            return Err(anyhow!(
                "thread '{name}' already exists; pick a different --name-prefix or omit it for an auto-generated prefix"
            ));
        }
    }

    // Parse `--evaluate` once; reuse for every attempt. We split on
    // ASCII whitespace, matching shell tokenization for the common
    // case (`cargo test`, `pytest -q`). Anything more exotic should
    // be wrapped in a script.
    let evaluate_cmd: Option<Vec<String>> = args
        .evaluate
        .as_ref()
        .map(|raw| raw.split_whitespace().map(|s| s.to_string()).collect());

    // Spawn the threads up front, on the main thread. Thread creation
    // touches the refs index and `.heddle/threads/`, which is a single
    // shared resource — serialising the registration step keeps the
    // race window from blowing up while the long-running per-attempt
    // work (running `<cmd>`) still happens in parallel below.
    let mut spawned: Vec<(usize, String, PathBuf)> = Vec::with_capacity(args.n as usize);
    let mut spawn_errors: Vec<AttemptResult> = Vec::new();
    for i in 1..=args.n {
        let name = format!("{prefix}-{i}");
        let start_args = ThreadStartArgs {
            name: name.clone(),
            from: None,
            path: None,
            workspace,
            agent_provider: None,
            agent_model: None,
            task: Some(format!(
                "attempt {i}/{n}: {cmd}",
                n = args.n,
                cmd = display_cmd(&args.command)
            )),
            parent_thread: None,
            automated: true,
            print_cd_path: false,
            daemon: true,
            no_daemon: false,
            shared_target,
        };
        match start_thread(&repo, start_args) {
            Ok(out) => {
                let path = out
                    .execution_path
                    .as_ref()
                    .map(PathBuf::from)
                    .ok_or_else(|| {
                        anyhow!("Could not determine ephemeral thread checkout path for '{name}'")
                    })?;
                spawned.push((i as usize, name, path));
            }
            Err(err) => {
                spawn_errors.push(AttemptResult {
                    index: i as usize,
                    thread: name,
                    status: AttemptStatus::SpawnError,
                    primary_exit_code: None,
                    primary_duration_secs: 0.0,
                    evaluate_exit_code: None,
                    evaluate_duration_secs: None,
                    captured_state: None,
                    diff_files: None,
                    thread_dropped: false,
                    note: Some(format!("failed to start ephemeral thread: {err}")),
                });
            }
        }
    }

    // Run the primary cmd in parallel across all spawned threads.
    // `std::thread::scope` lets us borrow `args.command` and the
    // evaluate cmd without `Arc`'ing them; the closure body is
    // self-contained, so borrowing keeps the code simpler.
    let primary_cmd: Vec<String> = args.command.clone();
    let evaluate_for_threads = evaluate_cmd.clone();

    let results: Arc<Mutex<Vec<AttemptResult>>> =
        Arc::new(Mutex::new(Vec::with_capacity(spawned.len())));

    std::thread::scope(|scope| {
        for (index, name, path) in &spawned {
            let results = Arc::clone(&results);
            let primary_cmd = &primary_cmd;
            let evaluate = evaluate_for_threads.as_ref();
            let name_owned = name.clone();
            let path_owned = path.clone();
            let index = *index;
            scope.spawn(move || {
                let attempt =
                    run_one_attempt(index, name_owned, &path_owned, primary_cmd, evaluate);
                results
                    .lock()
                    .expect("attempt result lock poisoned")
                    .push(attempt);
            });
        }
    });

    // Pull primary results out of the mutex; merge in the spawn-error
    // entries we collected above.
    let mut all_results: Vec<AttemptResult> =
        Arc::try_unwrap(results).unwrap().into_inner().unwrap();
    all_results.extend(spawn_errors);

    // Capture + diff-size pass. We open each thread's repo on the main
    // thread (capture writes to the shared object store and is cheap
    // relative to the cmd run; serialising avoids store-lock contention
    // with no measurable cost for N ≤ 10).
    for attempt in all_results.iter_mut() {
        if !matches!(
            attempt.status,
            AttemptStatus::Succeeded | AttemptStatus::EvaluateFailed
        ) {
            continue;
        }

        // Find the thread's checkout from the spawn list. SpawnError
        // entries never make it here (filtered above).
        let path = match spawned
            .iter()
            .find(|(idx, _, _)| *idx == attempt.index)
            .map(|(_, _, p)| p.clone())
        {
            Some(p) => p,
            None => continue,
        };

        let thread_repo = match Repository::open(&path) {
            Ok(r) => r,
            Err(err) => {
                attempt.note = Some(format!("could not open thread repo for capture: {err}"));
                continue;
            }
        };
        let user_config = UserConfig::load_default().unwrap_or_default();
        let intent = format!("attempt: {}", display_cmd(&primary_cmd));
        let snapshot = create_snapshot(
            &thread_repo,
            &user_config,
            Some(intent),
            Some(0.85),
            SnapshotAgentOverrides {
                provider: None,
                model: None,
                session: None,
                segment: None,
                policy: None,
                no_policy: false,
                no_agent: false,
            },
        );
        match snapshot {
            Ok(out) => {
                attempt.captured_state = Some(out.change_id.clone());
                // Compute diff against the parent's HEAD as the
                // tertiary ranking signal. We resolve the change-id
                // through the *parent* repo (the one with the original
                // HEAD); the captured_state is reachable from there
                // because both repos share the same object store.
                if let Some(parent_head) = parent_head_before.as_deref()
                    && let Ok(diff_count) = diff_file_count(&repo, parent_head, &out.change_id)
                {
                    attempt.diff_files = Some(diff_count);
                }
            }
            Err(err) => {
                // Capture failure is rare on a clean cmd-success path
                // (worktree empty, hook veto). Surface in the note
                // rather than failing the whole attempt.
                attempt.note = Some(format!("capture failed: {err}"));
            }
        }
    }

    // Drop the threads that should be cleaned up: anything with
    // SpawnError or Failed primary. EvaluateFailed and Succeeded both
    // stay around — the user might want to merge a "primary worked,
    // tests didn't pass yet" attempt anyway, after fixing the test.
    let mut dropped = 0usize;
    for attempt in all_results.iter_mut() {
        if matches!(
            attempt.status,
            AttemptStatus::Failed | AttemptStatus::SpawnError
        ) {
            // Skip drop for SpawnError — the thread never registered
            // properly, so `drop_thread_silent` would 404. The note
            // field already explains what happened.
            if matches!(attempt.status, AttemptStatus::SpawnError) {
                attempt.thread_dropped = false;
                continue;
            }
            match drop_thread_silent(&repo, &attempt.thread, true) {
                Ok(_) => {
                    attempt.thread_dropped = true;
                    dropped += 1;
                }
                Err(err) => {
                    tracing::warn!(
                        thread = %attempt.thread,
                        error = %err,
                        "drop failed during attempt cleanup"
                    );
                }
            }
        }
    }

    // Verify the parent's HEAD didn't drift. Same invariant as
    // `heddle try` without `--auto-merge`: parent is the user's
    // anchor; we never touch it.
    let parent_head_after = repo.head()?.map(|id| id.to_string_full());
    if parent_head_before != parent_head_after {
        return Err(anyhow!(
            "internal error: parent HEAD drifted during `heddle attempt` (before={:?} after={:?}); please file a bug",
            parent_head_before,
            parent_head_after
        ));
    }

    // Rank. Sort key documented in the module-level comment / plan:
    //   1. status (Succeeded < EvaluateFailed < Failed < SpawnError)
    //   2. primary exit code (0 first; any non-zero is a tie)
    //   3. evaluate exit code (None == 0 == 1?  see below)
    //   4. diff_files asc (smaller wins)
    //   5. duration asc (faster wins)
    //   6. index asc (stable tiebreaker)
    //
    // For (3) we treat a missing evaluate (i.e. user didn't pass
    // `--evaluate`) as "no signal", which sorts equal across all
    // attempts. When evaluate IS set, a 0 beats any non-zero.
    all_results.sort_by_key(rank_key);

    let attempts_total = all_results.len();
    let attempts_succeeded = all_results
        .iter()
        .filter(|a| matches!(a.status, AttemptStatus::Succeeded))
        .count();

    let recommended = all_results
        .iter()
        .find(|a| matches!(a.status, AttemptStatus::Succeeded))
        .or_else(|| {
            // Fallback: if --evaluate failed for everyone, surface the
            // best EvaluateFailed (still likely useful — the primary
            // worked).
            all_results
                .iter()
                .find(|a| matches!(a.status, AttemptStatus::EvaluateFailed))
        })
        .map(|a| a.thread.clone());

    let next_action = recommended
        .as_deref()
        .map(|name| format!("heddle merge {name} --with-diff"));

    let message = match &recommended {
        Some(thread) if attempts_succeeded > 0 => format!(
            "{attempts_succeeded}/{attempts_total} attempt(s) succeeded; recommended: {thread}"
        ),
        Some(thread) => {
            format!("no clean wins; best partial: {thread} (primary succeeded, --evaluate did not)")
        }
        None => format!("all {attempts_total} attempt(s) failed; nothing to recommend"),
    };

    let status = if recommended.is_some() {
        "completed"
    } else {
        "failed"
    };

    let output = AttemptOutput {
        status,
        action: "attempt",
        message,
        command: display_cmd(&primary_cmd),
        evaluate: evaluate_cmd.as_ref().map(|cmd| display_cmd(cmd)),
        attempts_total,
        attempts_succeeded,
        attempts_dropped: dropped,
        attempts: all_results,
        recommended,
        next_action,
    };

    emit(cli, &repo, &output)
}

/// Run a single attempt: primary cmd, optional evaluate cmd. Returns
/// a fully-populated `AttemptResult` minus the capture/diff fields,
/// which the main thread fills in afterwards.
fn run_one_attempt(
    index: usize,
    name: String,
    path: &Path,
    primary_cmd: &[String],
    evaluate: Option<&Vec<String>>,
) -> AttemptResult {
    // Run primary cmd. We capture its stdout/stderr to /dev/null so
    // N parallel attempts don't interleave their output on the user's
    // terminal — the ranking table is the user-visible signal. If a
    // user wants per-attempt logs they can wrap the cmd in a script
    // that tees to a file.
    let started = Instant::now();
    let primary_status = Command::new(&primary_cmd[0])
        .args(&primary_cmd[1..])
        .current_dir(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let primary_duration_secs = started.elapsed().as_secs_f64();

    let primary_status = match primary_status {
        Ok(s) => s,
        Err(err) => {
            return AttemptResult {
                index,
                thread: name,
                status: AttemptStatus::SpawnError,
                primary_exit_code: None,
                primary_duration_secs,
                evaluate_exit_code: None,
                evaluate_duration_secs: None,
                captured_state: None,
                diff_files: None,
                thread_dropped: false,
                note: Some(format!("failed to spawn primary cmd: {err}")),
            };
        }
    };

    let primary_exit_code = primary_status.code();
    if !primary_status.success() {
        return AttemptResult {
            index,
            thread: name,
            status: AttemptStatus::Failed,
            primary_exit_code,
            primary_duration_secs,
            evaluate_exit_code: None,
            evaluate_duration_secs: None,
            captured_state: None,
            diff_files: None,
            thread_dropped: false,
            note: None,
        };
    }

    // Primary succeeded. Run `--evaluate` if set.
    let (evaluate_exit_code, evaluate_duration_secs, status, note) = match evaluate {
        Some(cmd) if !cmd.is_empty() => {
            let started = Instant::now();
            let eval_status = Command::new(&cmd[0])
                .args(&cmd[1..])
                .current_dir(path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let eval_dur = started.elapsed().as_secs_f64();
            match eval_status {
                Ok(s) if s.success() => (s.code(), Some(eval_dur), AttemptStatus::Succeeded, None),
                Ok(s) => (
                    s.code(),
                    Some(eval_dur),
                    AttemptStatus::EvaluateFailed,
                    None,
                ),
                Err(err) => (
                    None,
                    Some(eval_dur),
                    AttemptStatus::EvaluateFailed,
                    Some(format!("failed to spawn --evaluate cmd: {err}")),
                ),
            }
        }
        _ => (None, None, AttemptStatus::Succeeded, None),
    };

    AttemptResult {
        index,
        thread: name,
        status,
        primary_exit_code,
        primary_duration_secs,
        evaluate_exit_code,
        evaluate_duration_secs,
        captured_state: None,
        diff_files: None,
        thread_dropped: false,
        note,
    }
}

/// Compute the parent ↔ thread-tip diff and return its file count. The
/// state IDs flow through `repo.resolve_state` so we accept either a
/// short or full change-id without re-deriving the prefix arithmetic.
fn diff_file_count(repo: &Repository, parent_head: &str, thread_tip: &str) -> Result<usize> {
    let from = repo
        .resolve_state(parent_head)?
        .ok_or_else(|| anyhow!("parent state '{}' not resolvable", parent_head))?;
    let to = repo
        .resolve_state(thread_tip)?
        .ok_or_else(|| anyhow!("thread state '{}' not resolvable", thread_tip))?;
    let diff = compute_state_diff(repo, &from, &to, false, 0)?;
    Ok(diff.changes.len())
}

/// Sort key for ranking. Lower values rank earlier.
fn rank_key(a: &AttemptResult) -> (u8, u8, u8, usize, u64, usize) {
    let status_rank = match a.status {
        AttemptStatus::Succeeded => 0,
        AttemptStatus::EvaluateFailed => 1,
        AttemptStatus::Failed => 2,
        AttemptStatus::SpawnError => 3,
    };
    let primary_rank = match a.primary_exit_code {
        Some(0) => 0u8,
        Some(_) => 1u8,
        None => 2u8,
    };
    let evaluate_rank = match a.evaluate_exit_code {
        Some(0) => 0u8,
        Some(_) => 1u8,
        // No evaluate run → neutral (sorts equal across attempts).
        None => 0u8,
    };
    // Smaller diffs win; missing diff sorts last (worst-case).
    let diff_rank = a.diff_files.unwrap_or(usize::MAX);
    // Convert duration to integer microseconds for stable Ord; missing
    // duration treated as 0 (only happens on spawn errors, which are
    // already last by status rank).
    let duration_micros = (a.primary_duration_secs * 1_000_000.0).round().max(0.0) as u64;
    (
        status_rank,
        primary_rank,
        evaluate_rank,
        diff_rank,
        duration_micros,
        a.index,
    )
}

/// `attempt-<8-hex>` derived from the cmd + a high-resolution
/// timestamp. Same shape as `try-<hash>` for symmetry.
fn default_attempt_prefix(command: &[String]) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    for arg in command {
        arg.hash(&mut hasher);
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        .hash(&mut hasher);
    let digest = hasher.finish();
    format!("attempt-{:08x}", digest as u32)
}

/// Render `<cmd>` for human messages.
fn display_cmd(cmd: &[String]) -> String {
    cmd.join(" ")
}

fn emit(cli: &Cli, repo: &Repository, output: &AttemptOutput) -> Result<()> {
    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(output)?);
        return Ok(());
    }

    // Human-readable comparison table.
    println!(
        "heddle attempt {} — `{}`",
        output.attempts_total, output.command
    );
    if let Some(eval) = &output.evaluate {
        println!("evaluate: `{}`", eval);
    }
    println!();

    // Header. Width chosen so a typical row (`attempt-x1y2z3-2`,
    // `12.3s`, `4 files`) fits in 100 cols. Thread name is the most
    // variable; pad to `max(name)` rather than a fixed width.
    let thread_width = output
        .attempts
        .iter()
        .map(|a| a.thread.len())
        .max()
        .unwrap_or(20)
        .max(20);
    // Column header note: "Δ files" is the count of files differing
    // between the parent's HEAD tree and the attempt thread's captured
    // tree. On materialized threads this can include checkout-metadata
    // artifacts that aren't the user's actual change set — useful as a
    // tertiary ranking signal, but read it as "delta against parent",
    // not "edits the command made". `heddle compare <parent> <attempt>`
    // gives the authoritative diff.
    println!(
        "  {:>4}  {:<thread_width$}  {:<10}  {:<10}  {:<10}  {:<10}  state",
        "rank",
        "thread",
        "primary",
        "evaluate",
        "Δ files",
        "duration",
        thread_width = thread_width,
    );
    for (rank, attempt) in output.attempts.iter().enumerate() {
        let primary = match attempt.status {
            AttemptStatus::SpawnError => "spawn-err".to_string(),
            _ => match attempt.primary_exit_code {
                Some(0) => "ok".to_string(),
                Some(code) => format!("exit {code}"),
                None => "signal".to_string(),
            },
        };
        let evaluate = match (output.evaluate.is_some(), attempt.evaluate_exit_code) {
            (false, _) => "-".to_string(),
            (true, Some(0)) => "ok".to_string(),
            (true, Some(code)) => format!("exit {code}"),
            (true, None) => "skipped".to_string(),
        };
        let diff = match attempt.diff_files {
            Some(n) => format!("{n} files"),
            None => "-".to_string(),
        };
        let duration = format!("{:.1}s", attempt.primary_duration_secs);
        let state = match (&attempt.captured_state, attempt.thread_dropped) {
            (Some(state), _) => style::change_id(state),
            (None, true) => "(dropped)".to_string(),
            (None, false) => "-".to_string(),
        };
        println!(
            "  {:>4}  {:<thread_width$}  {:<10}  {:<10}  {:<10}  {:<10}  {}",
            rank + 1,
            attempt.thread,
            primary,
            evaluate,
            diff,
            duration,
            state,
            thread_width = thread_width,
        );
    }
    println!();

    let painted = match output.status {
        "completed" => style::accent(&output.message),
        _ => style::warn(&output.message),
    };
    println!("{}", painted);
    if let Some(next) = &output.next_action {
        println!("Next: {}", style::bold(next));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use objects::object::ChangeId;

    use super::*;

    fn init_repo() -> (tempfile::TempDir, Repository) {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        (temp, repo)
    }

    #[test]
    fn cmd_attempt_preflight_refuses_when_any_synthesized_name_collides() {
        // Set up: a thread ref already exists at `attempt-fixed-1`
        // (the first synthesized name `cmd_attempt` would produce
        // with `--name-prefix attempt-fixed`). The preflight must
        // refuse with the precise collision message BEFORE spawning
        // any of the three attempts — otherwise `start_thread` would
        // create-or-resume into the existing thread and the failure
        // path could later drop it.
        let (_temp, repo) = init_repo();
        let id = ChangeId::generate();
        repo.refs().set_thread("attempt-fixed-1", &id).unwrap();

        let make_args = || AttemptArgs {
            n: 3,
            workspace: WorkspaceModeArg::Heavy,
            shared_target: false,
            no_shared_target: true, // force off so we don't depend on workspace detection
            name_prefix: Some("attempt-fixed".into()),
            evaluate: None,
            command: vec!["true".into()],
        };
        let cli = Cli {
            command: crate::cli::Commands::Attempt(make_args()),
            json: false,
            output: None,
            no_color: true,
            repo: Some(repo.root().to_path_buf()),
            verbose: 0,
            quiet: false,
            op_id: None,
        };

        let err = cmd_attempt(&cli, make_args()).expect_err("must refuse on name collision");
        let msg = err.to_string();
        assert!(
            msg.contains("attempt-fixed-1") && msg.contains("already exists"),
            "expected precise collision message naming attempt-fixed-1; got: {msg}"
        );
        assert!(
            msg.contains("--name-prefix"),
            "message should point at --name-prefix as the fix; got: {msg}"
        );

        // Critical: the all-or-nothing contract — no NEW threads got
        // created. attempt-fixed-1 still exists (the one we planted),
        // attempt-fixed-2 and attempt-fixed-3 must not.
        assert!(
            repo.refs().get_thread("attempt-fixed-1").unwrap().is_some(),
            "the planted ref must still be there"
        );
        assert!(
            repo.refs().get_thread("attempt-fixed-2").unwrap().is_none(),
            "preflight must refuse before any new threads are spawned"
        );
        assert!(
            repo.refs().get_thread("attempt-fixed-3").unwrap().is_none(),
            "preflight must refuse before any new threads are spawned"
        );
    }

    #[test]
    fn cmd_attempt_preflight_refuses_when_middle_name_collides() {
        // Verify all-or-nothing: even if the FIRST name is free and
        // only `<prefix>-2` collides, we must refuse before spawning
        // `<prefix>-1`.
        let (_temp, repo) = init_repo();
        let id = ChangeId::generate();
        repo.refs().set_thread("attempt-mid-2", &id).unwrap();

        let make_args = || AttemptArgs {
            n: 3,
            workspace: WorkspaceModeArg::Heavy,
            shared_target: false,
            no_shared_target: true,
            name_prefix: Some("attempt-mid".into()),
            evaluate: None,
            command: vec!["true".into()],
        };
        let cli = Cli {
            command: crate::cli::Commands::Attempt(make_args()),
            json: false,
            output: None,
            no_color: true,
            repo: Some(repo.root().to_path_buf()),
            verbose: 0,
            quiet: false,
            op_id: None,
        };

        let err = cmd_attempt(&cli, make_args()).expect_err("must refuse on mid-collision");
        assert!(
            err.to_string().contains("attempt-mid-2"),
            "must name the colliding thread, not just the first; got: {err}"
        );
        // Crucially attempt-mid-1 was NOT created — preflight ran
        // before any spawn.
        assert!(
            repo.refs().get_thread("attempt-mid-1").unwrap().is_none(),
            "all-or-nothing: no new threads spawn on preflight failure"
        );
    }
}