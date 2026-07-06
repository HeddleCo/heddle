// SPDX-License-Identifier: Apache-2.0
//! `heddle try -- <cmd>` — atomic-ephemeral-thread sugar.
//!
//! Implements item 3.1 of the heddle 6→8 plan: spin up an ephemeral
//! thread with an isolated checkout, run `<cmd>` inside that checkout,
//! and either land a captured state on success or roll everything back
//! on failure. The user's parent-thread working tree is the
//! load-bearing invariant — it MUST end in exactly the state it
//! started, regardless of whether `<cmd>` succeeded or failed.
//!
//! ### Working-tree invariant — how it's enforced
//!
//! 1. We never touch the parent's working tree. The cmd runs with
//!    `Command::current_dir(&thread.execution_path)`; the child
//!    inherits a cwd inside the ephemeral thread's isolated checkout
//!    and any writes land there.
//! 2. Capture happens against the *thread's* repo (opened from
//!    `thread.execution_path`), not the parent. The parent's HEAD
//!    only advances when `--auto-merge` is set and the merge
//!    succeeds.
//! 3. Failure path: drop the ephemeral thread (best-effort), then
//!    surface the original cmd's exit code via `process::exit`. We
//!    never return after spawning the cmd unless we either captured
//!    cleanly or the failure was funneled through the drop step.
//!
//! ### File naming
//!
//! `try.rs` is a Rust keyword and would force every reference to use
//! `r#try`. The module is named `try_cmd.rs` and exposed as `cmd_try`
//! for symmetry with the rest of the CLI. The user-visible verb is
//! still `heddle try`.

use std::{
    path::PathBuf,
    process::{Command, Stdio},
    time::Instant,
};

use anyhow::{Result, anyhow};
use repo::{Repository, ThreadManager, shell_quote};
use serde::Serialize;

use super::{
    action_line::print_next,
    advice::RecoveryAdvice,
    child_env::sanitized_child_env,
    command_catalog::{ActionFields, ActionTemplate},
    verification_health::action_templates,
    merge::merge_thread_into_current,
    snapshot::{SnapshotAgentOverrides, create_snapshot},
    thread::start_thread,
    thread_cmd::{DropOutcome, drop_thread_silent},
};
use crate::{
    cli::{Cli, ThreadStartArgs, TryArgs, WorkspaceModeArg, should_output_json, style},
    config::UserConfig,
};

/// What `heddle try` returns to JSON consumers. Mirrors
/// `OperatorCommandOutput` shape (status / action / message) plus the
/// fields specific to a try (the ephemeral thread, the cmd's exit,
/// the captured state if any).
#[derive(Debug, Serialize)]
struct TryOutput {
    /// `"completed"` on a clean success path (zero exit + capture
    /// landed). `"failed"` when the user's command exited non-zero.
    status: &'static str,
    action: &'static str,
    message: String,

    /// The ephemeral thread Heddle created.
    thread: String,
    /// `true` when the thread was dropped at the end (either because
    /// the cmd failed, or because `--auto-merge` consumed it).
    thread_dropped: bool,

    /// When cleanup of the ephemeral thread fails (lock contention,
    /// filesystem error, etc.) on a path where we tried to drop it,
    /// this carries the error message so automation can detect the
    /// orphan instead of relying on `thread_dropped` alone. `None` when
    /// no cleanup was attempted, or when cleanup succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    cleanup_error: Option<String>,

    /// The exit code observed from `<cmd>`. `None` when the process
    /// was killed by a signal.
    exit_code: Option<i32>,
    /// Wall-clock duration of `<cmd>` in milliseconds.
    duration_ms: u128,

    /// The captured state on the ephemeral thread, when one landed.
    /// `None` on the failure path or when capture itself failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    captured_state: Option<String>,

    /// When `--auto-merge` is set and the merge ran, this holds the
    /// merge state's change-id on the parent thread. Pulled from the
    /// merge command's structured output.
    #[serde(skip_serializing_if = "Option::is_none")]
    merge_state: Option<String>,

    /// Hint surfaced to the user when `--auto-merge` is *not* set:
    /// the exact merge preview command they should run. Always
    /// printed in non-JSON mode; included for JSON consumers so the
    /// agent doesn't have to reconstruct the verb.
    #[serde(skip_serializing_if = "Option::is_none")]
    next_action: Option<String>,
    next_action_template: Option<ActionTemplate>,

    /// Same primary command as `next_action`, under the cross-command
    /// verification/action field name agents already inspect.
    #[serde(skip_serializing_if = "Option::is_none")]
    recommended_action: Option<String>,
    recommended_action_template: Option<ActionTemplate>,

    /// Secondary safe commands. For a successful non-auto-merge try,
    /// the primary action lands the thread and this command discards
    /// it. Keeping them separate makes every emitted action parseable.
    recovery_commands: Vec<String>,
    recovery_action_templates: Vec<ActionTemplate>,
}

pub fn cmd_try(cli: &Cli, args: TryArgs) -> Result<()> {
    if args.command.is_empty() {
        return Err(anyhow!(RecoveryAdvice::invalid_usage(
            "try_command_required",
            "Usage: heddle try -- <cmd...>",
            "Pass a command after `--` so Heddle can run it inside an ephemeral thread.",
            "heddle try -- <cmd...>",
        )));
    }

    let repo_root_arg = cli
        .repo
        .as_ref()
        .cloned()
        .unwrap_or(std::env::current_dir()?);
    let repo = Repository::open(&repo_root_arg)?;

    // Snapshot the parent's HEAD up front. The parent's HEAD must not
    // move unless `--auto-merge` advances it explicitly. We compare
    // against this at the end as the invariant check (the parent's
    // working tree is also untouched because we never `cd` into it).
    let parent_head_before = repo.head()?.map(|id| id.to_string_full());

    let thread_name = args
        .name
        .clone()
        .unwrap_or_else(|| default_try_name(&args.command));

    // Reject collisions with an existing thread up front. Without
    // this guard, `start_thread` is create-or-resume: a `heddle try
    // --name <existing>` would attach to the user's real thread, and
    // the failure-path `drop_thread_silent` later in this function
    // would then abandon it. That's a footgun even when the run
    // succeeds (we'd silently mutate the existing thread's state).
    // Only the user-supplied `--name` path needs the check —
    // auto-generated names embed a uuid and won't collide.
    if args.name.is_some() && thread_name_in_use(&repo, &thread_name)? {
        return Err(anyhow!(try_thread_name_collision_advice(&thread_name)));
    }

    // Use start_thread directly so the ephemeral thread is registered
    // exactly the same way `heddle start` does. `auto` resolves to a
    // materialized checkout here: virtualized mounts are awkward to
    // execute commands inside, and a real checkout is what the cmd
    // expects.
    let workspace = match args.workspace {
        WorkspaceModeArg::Auto => WorkspaceModeArg::Materialized,
        other => other,
    };
    let start_args = ThreadStartArgs {
        name: thread_name.clone(),
        from: None,
        path: None,
        workspace,
        agent_provider: None,
        agent_model: None,
        task: Some(format!("try: {}", display_cmd(&args.command))),
        parent_thread: None,
        automated: true,
        print_cd_path: false,
        daemon: true,
        no_daemon: false,
        shared_target: false,
        hydrate: false,
    };
    let start_output = start_thread(&repo, start_args)?;
    let thread_path = start_output
        .execution_path
        .as_ref()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("Could not determine ephemeral thread checkout path"))?;

    // Run the cmd inside the thread's checkout.
    //
    // The child gets a sanitized environment (`env_clear()` + the
    // shared allowlist) so no inherited `GIT_*` overlay var or ambient
    // secret leaks into the user's command — mirrors `heddle run`.
    // Heddle's own thread/session context is re-injected explicitly.
    let started = Instant::now();
    let mut child = Command::new(&args.command[0]);
    child
        .args(&args.command[1..])
        .current_dir(&thread_path)
        .env_clear()
        .envs(sanitized_child_env())
        .env("HEDDLE_THREAD_NAME", &thread_name);
    if let Some(summary) = &start_output.thread {
        if let Some(session) = &summary.session_id {
            child.env("HEDDLE_SESSION_ID", session);
        }
        if let Some(heddle_session) = &summary.heddle_session_id {
            child.env("HEDDLE_SESSION_SEGMENT", heddle_session);
        }
    }
    let status = child
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();
    let duration_ms = started.elapsed().as_millis();

    let exit = match status {
        Ok(status) => status,
        Err(err) => {
            // The cmd never even started (program-not-found, etc.).
            // This is not a "the user's code is broken" failure — it's
            // an operator error. Drop the thread and surface a real
            // error so the caller can fix the invocation.
            let _ = drop_thread_silent(&repo, &thread_name, true, true);
            return Err(anyhow!(
                "Failed to execute `{}`: {}",
                display_cmd(&args.command),
                err
            ));
        }
    };

    let exit_code = exit.code();

    if !exit.success() {
        // Failure path. Drop the thread and exit with the cmd's code.
        // We deliberately use a best-effort drop here: if the drop
        // fails (e.g. lock contention), we still surface the cmd's
        // failure rather than masking it with a teardown error — but
        // we report the cleanup failure honestly in the JSON shape
        // and as a stderr warning, so automation isn't fooled into
        // thinking the orphan ephemeral thread was cleaned up.
        let drop_result = drop_thread_silent(&repo, &thread_name, true, true);
        let (thread_dropped, cleanup_error) =
            interpret_drop_result(&thread_name, drop_result, "try cleanup");

        // Verify the parent's HEAD didn't drift. If it did, that's a
        // real bug in this code path; we surface it loudly rather
        // than hiding behind the cmd's exit code.
        verify_parent_unchanged(&repo, parent_head_before.as_deref())?;

        let drop_msg = if thread_dropped {
            format!("thread '{thread_name}' dropped")
        } else {
            format!("thread '{thread_name}' NOT dropped (cleanup failed)")
        };
        let recovery_commands = if thread_dropped {
            Vec::new()
        } else {
            vec![format!("heddle thread drop {thread_name}")]
        };
        let output = TryOutput {
            status: "failed",
            action: "try",
            message: format!(
                "`{}` failed (exit {}); {}",
                display_cmd(&args.command),
                exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into()),
                drop_msg
            ),
            thread: thread_name.clone(),
            thread_dropped,
            cleanup_error,
            exit_code,
            duration_ms,
            captured_state: None,
            merge_state: None,
            next_action: None,
            next_action_template: None,
            recommended_action: None,
            recommended_action_template: None,
            recovery_action_templates: action_templates(&recovery_commands),
            recovery_commands,
        };
        emit(cli, &repo, &output)?;
        // Exit with the cmd's exit code — this is the contract: try
        // passes through the failure mode of the wrapped program.
        std::process::exit(exit_code.unwrap_or(1));
    }

    // Success path. Capture the thread's state.
    //
    // We open the *thread's* repo (rather than the parent's) so the
    // capture lands on the thread's HEAD. The thread is a full
    // Heddle-managed checkout; opening it gives us a Repository
    // pointing at the same store but anchored on the thread's HEAD.
    let thread_repo = Repository::open(&thread_path)?;
    let user_config = UserConfig::load_default()?;
    let intent = format!("try: {}", display_cmd(&args.command));
    // Confidence picks up a small bump when --auto-merge is set —
    // the user is implicitly stating "I'm comfortable letting this
    // land". Without --auto-merge we stay conservative at 0.85.
    let confidence = if args.auto_merge { 0.9 } else { 0.85 };
    let snapshot = create_snapshot(
        &thread_repo,
        &user_config,
        Some(intent),
        Some(confidence),
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
    let captured_state = match snapshot {
        Ok(out) => Some(out.change_id),
        Err(err) => {
            // Capture failed despite the cmd succeeding (e.g. nothing
            // changed in the worktree, or a hook vetoed). We don't
            // tear down the thread on this branch — the user might
            // want to inspect it. Surface the warning and continue.
            tracing::warn!(error = %err, "capture failed in try thread; leaving thread in place");
            None
        }
    };

    // Auto-merge if requested and capture succeeded.
    let mut merge_state: Option<String> = None;
    let mut thread_dropped = false;
    let mut cleanup_error: Option<String> = None;
    if args.auto_merge && captured_state.is_some() {
        let merge_output = merge_thread_into_current(
            &repo,
            &thread_name,
            Some(format!("try: {}", display_cmd(&args.command))),
            false,
            false,
            true,
            false,
            false,
        )?;
        merge_state = merge_output.merge_state.clone();

        // Drop the thread after a clean merge unless the user asked
        // us to keep it. The merge has already moved the parent's
        // HEAD; the thread's checkout is a leftover sandbox at this
        // point. Defensive guard: only drop on a clean merge — should
        // never fire as non-clean given `preview=false`, but being
        // explicit keeps the failure mode obvious.
        if !args.keep_on_success && merge_output.conflicts.is_empty() {
            let drop_result = drop_thread_silent(&repo, &thread_name, true, true);
            let (dropped, err) =
                interpret_drop_result(&thread_name, drop_result, "auto-merge cleanup");
            thread_dropped = dropped;
            cleanup_error = err;
        }
    }

    // Final invariant check: if --auto-merge was *not* set, the
    // parent's HEAD must equal what it was before we started.
    if !args.auto_merge {
        verify_parent_unchanged(&repo, parent_head_before.as_deref())?;
    }

    let next_action = if !args.auto_merge {
        // Quote defensively at construction (heddle#464 defense-in-depth): a
        // thread id flows into the validated next_action / recommended_action
        // fields, so an unsafe one must render as a single shell token, never
        // bare. A clean slug passes through unchanged.
        Some(format!(
            "heddle ready --thread {}",
            shell_quote(&thread_name)
        ))
    } else {
        None
    };
    let recommended_action = next_action.clone();
    let recommended_action_fields =
        ActionFields::from_optional_action_ref(recommended_action.as_deref());
    let recovery_commands = if !args.auto_merge || !thread_dropped {
        vec![format!("heddle thread drop {thread_name}")]
    } else {
        Vec::new()
    };
    let recovery_action_templates = action_templates(&recovery_commands);

    let message = if args.auto_merge {
        match (&captured_state, &merge_state) {
            (Some(state), Some(merge)) => format!(
                "`{}` succeeded; captured {}, merged into parent as {}",
                display_cmd(&args.command),
                state,
                merge
            ),
            (Some(state), None) => format!(
                "`{}` succeeded; captured {}, merge into parent skipped",
                display_cmd(&args.command),
                state
            ),
            _ => format!(
                "`{}` succeeded; nothing to capture",
                display_cmd(&args.command)
            ),
        }
    } else {
        match &captured_state {
            Some(state) => format!(
                "`{}` succeeded; thread '{}' ready (state {}). Check readiness with `heddle ready --thread {}` before landing.",
                display_cmd(&args.command),
                thread_name,
                state,
                thread_name
            ),
            None => format!(
                "`{}` succeeded; thread '{}' ready (no capture).",
                display_cmd(&args.command),
                thread_name
            ),
        }
    };

    let output = TryOutput {
        status: "completed",
        action: "try",
        message,
        thread: thread_name,
        thread_dropped,
        cleanup_error,
        exit_code,
        duration_ms,
        captured_state,
        merge_state,
        next_action,
        next_action_template: recommended_action_fields.template.clone(),
        recommended_action,
        recommended_action_template: recommended_action_fields.template,
        recovery_commands,
        recovery_action_templates,
    };
    emit(cli, &repo, &output)
}

/// Check whether a thread name is in use by ANY route that
/// `start_thread` would resume from. Returns `Ok(true)` if the name
/// resolves to an existing thread via either:
///   1. `ThreadManager::find_by_thread` / `ThreadManager::load`
///      — covers locally-registered threads with a manager record.
///   2. `repo.refs().get_thread` — covers ref-only threads (e.g.
///      legacy or synced repos where a ref exists without a
///      corresponding manager record).
///
/// The `start_thread` create-or-resume contract reads from both
/// sources, so the guard must too — otherwise a manager-less ref can
/// silently slip past a `--name`/`--name-prefix` check and land us
/// attached to (and later dropping) a real existing thread.
pub(crate) fn thread_name_in_use(repo: &Repository, name: &str) -> Result<bool> {
    let manager = ThreadManager::new(repo.heddle_dir());
    if manager.find_by_thread(name)?.is_some() || manager.load(name)?.is_some() {
        return Ok(true);
    }
    if repo
        .refs()
        .get_thread(&objects::object::ThreadName::new(name))?
        .is_some()
    {
        return Ok(true);
    }
    Ok(false)
}

/// Confirm the parent's HEAD didn't drift while we were running. The
/// invariant is "parent worktree unchanged on every path other than
/// `--auto-merge`". A drift here is a bug in this command, not user
/// error; surface it loudly.
fn verify_parent_unchanged(repo: &Repository, before: Option<&str>) -> Result<()> {
    let after = repo.head()?.map(|id| id.to_string_full());
    let after_ref = after.as_deref();
    if before != after_ref {
        return Err(anyhow!(
            "internal error: parent HEAD drifted during `heddle try` (before={:?} after={:?}); please file a bug",
            before,
            after_ref
        ));
    }
    Ok(())
}

/// Render `<cmd>` for human messages. `display_cmd(["cargo", "test"])`
/// returns `"cargo test"`. Quoting is intentionally simple — the user
/// can run with `--output json` for a structured shape.
fn display_cmd(cmd: &[String]) -> String {
    cmd.join(" ")
}

fn try_thread_name_collision_advice(thread_name: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "try_thread_name_collision",
        format!("thread '{thread_name}' already exists"),
        "Pick a different `--name`, or omit it so Heddle can generate a collision-resistant name.",
        format!("`heddle try --name {thread_name}` would target an existing thread"),
        "reusing that thread name could attach to and later clean up an existing user thread",
        "no try thread was spawned and the existing thread was left unchanged",
        "heddle try --name <different-name> -- <cmd...>",
        vec![
            "heddle try --name <different-name> -- <cmd...>".to_string(),
            "heddle try -- <cmd...>".to_string(),
        ],
    )
}

/// Build the default thread name from the cmd. `try-<8-hex>` of a
/// hash over the cmd vector + a high-resolution timestamp. The
/// timestamp ensures back-to-back `heddle try -- true` invocations
/// don't collide.
fn default_try_name(command: &[String]) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    for arg in command {
        arg.hash(&mut hasher);
    }
    // Mix in a monotonic-ish nonce so two `heddle try -- true` calls
    // back-to-back generate distinct names.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        .hash(&mut hasher);
    let digest = hasher.finish();
    format!("try-{:08x}", digest as u32)
}

fn emit(cli: &Cli, repo: &Repository, output: &TryOutput) -> Result<()> {
    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(output)?);
    } else {
        let painted = match output.status {
            "completed" => style::accent(&output.message),
            _ => style::warn(&output.message),
        };
        println!("{}", painted);
        if let Some(next) = &output.next_action {
            print_next(next);
        }
        if let Some(discard) = output.recovery_commands.first() {
            println!("Discard: {}", style::bold(discard));
        }
    }
    Ok(())
}

/// Decide what to surface when `drop_thread_silent` either succeeds or
/// fails on a path where we tried to drop the ephemeral thread. Returns
/// `(thread_dropped, cleanup_error)` so the caller can plug them into
/// `TryOutput` directly. On error we also emit a stderr warning so
/// interactive users see what happened — automation reads
/// `cleanup_error` from the JSON shape.
fn interpret_drop_result(
    thread_name: &str,
    result: Result<DropOutcome>,
    context: &str,
) -> (bool, Option<String>) {
    match result {
        Ok(_) => (true, None),
        Err(err) => {
            let msg = err.to_string();
            tracing::warn!(thread = %thread_name, error = %err, context, "drop failed");
            eprintln!(
                "warning: failed to drop ephemeral thread '{thread_name}' during {context}: {msg}"
            );
            (false, Some(msg))
        }
    }
}

#[cfg(test)]
mod tests {
    use objects::object::{ChangeId, ThreadName};

    use super::*;

    fn init_repo() -> (tempfile::TempDir, Repository) {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        (temp, repo)
    }

    #[test]
    fn thread_name_in_use_returns_false_for_unknown_name() {
        let (_temp, repo) = init_repo();
        assert!(!thread_name_in_use(&repo, "no-such-thread").unwrap());
    }

    #[test]
    fn thread_name_in_use_detects_ref_only_thread() {
        // Write a thread ref directly with NO ThreadManager record —
        // the legacy / synced-repo shape that the previous guard
        // missed. `thread_name_in_use` must catch it.
        let (_temp, repo) = init_repo();
        let id = ChangeId::generate();
        repo.refs()
            .set_thread(&ThreadName::new("ref-only-thread"), &id)
            .unwrap();

        // ThreadManager has no record (we didn't go through start_thread).
        let manager = ThreadManager::new(repo.heddle_dir());
        assert!(manager.find_by_thread("ref-only-thread").unwrap().is_none());
        assert!(manager.load("ref-only-thread").unwrap().is_none());

        // The helper still refuses — the ref-level lookup catches it.
        assert!(thread_name_in_use(&repo, "ref-only-thread").unwrap());
    }

    #[test]
    fn cmd_try_refuses_name_collision_via_ref_only_thread() {
        // End-to-end shape of Fix 2: a thread ref that exists without
        // a manager record (legacy / synced repo) must cause `heddle
        // try --name <that-ref-name>` to refuse before it ever calls
        // start_thread. We invoke cmd_try with all the args wired up;
        // the guard short-circuits with the precise message.
        let (_temp, repo) = init_repo();
        let id = ChangeId::generate();
        repo.refs()
            .set_thread(&ThreadName::new("legacy-ref-thread"), &id)
            .unwrap();

        let make_args = || TryArgs {
            name: Some("legacy-ref-thread".into()),
            workspace: WorkspaceModeArg::Materialized,
            auto_merge: false,
            keep_on_success: false,
            command: vec!["true".into()],
        };
        let cli = Cli {
            command: crate::cli::Commands::Try(make_args()),
            output: None,
            no_color: true,
            repo: Some(repo.root().to_path_buf()),
            verbose: 0,
            quiet: false,
            op_id: None,
        };
        let err = cmd_try(&cli, make_args()).expect_err("must refuse ref-only collision");
        let advice = err
            .chain()
            .find_map(|cause| cause.downcast_ref::<RecoveryAdvice>())
            .expect("try collision refusal should carry typed recovery advice");
        assert_eq!(advice.kind, "try_thread_name_collision");
        let msg = err.to_string();
        assert!(
            msg.contains("legacy-ref-thread") && msg.contains("already exists"),
            "expected precise collision message; got: {msg}"
        );
    }

    #[test]
    fn interpret_drop_result_ok_marks_dropped_with_no_cleanup_error() {
        let (dropped, cleanup_error) =
            interpret_drop_result("ephemeral-x", Ok(DropOutcome::Deleted), "try cleanup");
        assert!(dropped);
        assert!(cleanup_error.is_none());
    }

    #[test]
    fn interpret_drop_result_err_marks_not_dropped_and_carries_message() {
        let err: Result<DropOutcome> = Err(anyhow!(
            "simulated lock contention on .heddle/locks/threads"
        ));
        let (dropped, cleanup_error) = interpret_drop_result("ephemeral-x", err, "try cleanup");
        assert!(!dropped, "thread_dropped must be false when cleanup fails");
        let msg = cleanup_error.expect("cleanup_error must carry the failure message");
        assert!(
            msg.contains("simulated lock contention"),
            "cleanup_error should include the underlying message; got: {msg}"
        );
    }

    #[test]
    fn try_output_serializes_cleanup_error_only_when_present() {
        // Field-shape contract: when cleanup_error is None it must NOT
        // appear in the JSON (skip_serializing_if). When it IS Some it
        // must appear alongside thread_dropped: false. Automation
        // depends on this exact shape — a misleading thread_dropped:
        // true with no cleanup_error would mask an orphan ephemeral
        // thread.
        let ok_output = TryOutput {
            status: "failed",
            action: "try",
            message: "ok-path".into(),
            thread: "t".into(),
            thread_dropped: true,
            cleanup_error: None,
            exit_code: Some(1),
            duration_ms: 0,
            captured_state: None,
            merge_state: None,
            next_action: None,
            next_action_template: None,
            recommended_action: None,
            recommended_action_template: None,
            recovery_commands: Vec::new(),
            recovery_action_templates: Vec::new(),
        };
        let json = serde_json::to_string(&ok_output).unwrap();
        assert!(
            !json.contains("cleanup_error"),
            "field must be skipped when None: {json}"
        );
        assert!(json.contains("\"thread_dropped\":true"));

        let err_output = TryOutput {
            status: "failed",
            action: "try",
            message: "err-path".into(),
            thread: "t".into(),
            thread_dropped: false,
            cleanup_error: Some("lock held".into()),
            exit_code: Some(1),
            duration_ms: 0,
            captured_state: None,
            merge_state: None,
            next_action: None,
            next_action_template: None,
            recommended_action: None,
            recommended_action_template: None,
            recovery_commands: Vec::new(),
            recovery_action_templates: Vec::new(),
        };
        let json = serde_json::to_string(&err_output).unwrap();
        assert!(
            json.contains("\"thread_dropped\":false"),
            "thread_dropped must be false when cleanup failed: {json}"
        );
        assert!(
            json.contains("\"cleanup_error\":\"lock held\""),
            "cleanup_error must surface the message: {json}"
        );
    }
}
