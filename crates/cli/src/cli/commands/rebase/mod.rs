// SPDX-License-Identifier: Apache-2.0
//! Rebase command - replay commits onto another thread.

use std::fs;

use anyhow::{Result, anyhow};
use objects::object::ThreadName;
use refs::Head;
use repo::Repository;
use serde_json::json;

use super::{
    action_line::print_next_step,
    advice::RecoveryAdvice,
    ff_record::record_ff_advance,
    git_overlay_health::{
        RepositoryVerificationState, action_template, build_repository_verification_state,
        repository_verification_primary_command,
    },
    snapshot::ensure_current_state,
    worktree_safety::ensure_worktree_clean,
};
use crate::{
    cli::{Cli, should_output_json},
    config::UserConfig,
};

mod rebase_ops;
mod rebase_state;

use rebase_ops::{
    flush_rebase_batch, mint_rebase_transaction_id, replay_commits, replay_commits_silent,
};

use super::ff_record::ff_advance_deferred;
pub(crate) use rebase_state::load_rebase_state as load_persisted_rebase_state;
use rebase_state::{
    RebaseState, collect_commits_to_rebase, is_ancestor_of, load_rebase_state,
    load_rebase_state_for_abort, save_rebase_state,
};

const REBASE_STATE_FILE: &str = "REBASE_STATE";

pub(crate) enum OperatorContinueStatus {
    Continued,
    Completed,
    Blocked,
}

pub fn cmd_rebase(
    cli: &Cli,
    thread: Option<&str>,
    abort: bool,
    cont: bool,
    force: bool,
) -> Result<()> {
    // Same metadata-resolution pattern as `cmd_merge`: open at CWD to
    // discover the active thread, then re-open at that thread's
    // metadata-recorded worktree so commits are replayed into the
    // thread's actual checkout. See `Repository::active_worktree_path`
    // for fallback semantics.
    let cwd_repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let target_path = cwd_repo.active_worktree_path()?;
    let repo = if target_path == *cwd_repo.root() {
        cwd_repo
    } else {
        Repository::open(&target_path)?
    };

    // Rebase replays commits onto another thread by mutating the worktree
    // (fast-forward via `fast_forward_attached` flows through
    // `plan_worktree_apply`, which silently auto-falls back to
    // `FullRematerialize` on a dirty worktree — that wipes uncommitted
    // edits). The guard runs only on the entry path (not `--abort` /
    // `--continue`, which need access to the in-progress state on disk).
    if !force && !abort && !cont {
        ensure_worktree_clean(&repo, "rebase")?;
    }

    run_rebase(&repo, thread, abort, cont, Some(cli))
}

pub(crate) fn cmd_rebase_silent(
    repo: &Repository,
    thread: Option<&str>,
    abort: bool,
    cont: bool,
) -> Result<()> {
    run_rebase(repo, thread, abort, cont, None)
}

pub(crate) fn continue_rebase_for_operator(repo: &Repository) -> Result<OperatorContinueStatus> {
    let rebase_state_path = repo.heddle_dir().join(REBASE_STATE_FILE);
    if !rebase_state_path.exists() {
        return Err(anyhow!(no_rebase_in_progress_advice("continue rebase")));
    }

    let before = load_rebase_state(&rebase_state_path)?;
    if let Some(pre_conflict_head) = before.pre_conflict_head {
        let current_state = repo
            .current_state()?
            .ok_or_else(|| anyhow!("No current state"))?;
        if current_state.change_id != pre_conflict_head
            && Some(current_state.change_id) != before.pending_manual_resolution
        {
            let current_tree = repo
                .store()
                .get_tree(&current_state.tree)?
                .ok_or_else(|| anyhow!("Current state tree not found"))?;
            let worktree_status = repo.compare_worktree_cached(&current_tree)?;
            let worktree_is_clean = worktree_status.modified.is_empty()
                && worktree_status.added.is_empty()
                && worktree_status.deleted.is_empty();
            if !worktree_is_clean {
                return Ok(OperatorContinueStatus::Blocked);
            }
        }
    }
    let before_index = before.current_index;
    let before_pending_manual_resolution = before.pending_manual_resolution;

    cmd_rebase_silent(repo, None, false, true)?;

    if !rebase_state_path.exists() {
        return Ok(OperatorContinueStatus::Completed);
    }

    let after = load_rebase_state(&rebase_state_path)?;
    if after.pending_manual_resolution.is_some()
        && after.current_index == before_index
        && after.pending_manual_resolution == before_pending_manual_resolution
    {
        return Ok(OperatorContinueStatus::Blocked);
    }

    Ok(OperatorContinueStatus::Continued)
}

pub(crate) fn has_persisted_rebase_state(repo: &Repository) -> bool {
    repo.heddle_dir().join(REBASE_STATE_FILE).exists()
}

fn run_rebase(
    repo: &Repository,
    thread: Option<&str>,
    abort: bool,
    cont: bool,
    cli: Option<&Cli>,
) -> Result<()> {
    let rebase_state_path = repo.heddle_dir().join(REBASE_STATE_FILE);

    if abort {
        return handle_abort(repo, &rebase_state_path, cli);
    }

    if cont {
        return handle_continue(repo, &rebase_state_path, cli);
    }

    let target_thread = thread.ok_or_else(rebase_target_required_advice)?;

    let current_change = ensure_current_state(
        repo,
        &UserConfig::load_default().unwrap_or_default(),
        Some(format!(
            "Bootstrap git-overlay before rebasing onto {}",
            target_thread
        )),
    )?;
    let current_state = repo
        .store()
        .get_state(&current_change)?
        .ok_or_else(|| anyhow!("Current state not found"))?;

    let target_change_id = repo
        .refs()
        .get_thread(&ThreadName::new(target_thread))?
        .ok_or_else(|| rebase_target_not_found_advice(target_thread))?;

    if current_state.change_id == target_change_id {
        emit_up_to_date_if_trusted(repo, cli)?;
        return Ok(());
    }

    let is_ancestor = is_ancestor_of(repo, &current_state.change_id, &target_change_id)?;

    if is_ancestor {
        // Wrap the single-FF arm in the same TransactionCommit-bracketed
        // batch shape replay_commits uses, so `heddle undo` treats this
        // path identically to a multi-commit rebase (heddle#198).
        let advance = ff_advance_deferred(repo, target_thread, &target_change_id)?;
        flush_rebase_batch(repo, &[advance], &mint_rebase_transaction_id())?;

        if let Some(cli) = cli
            && should_output_json(cli, Some(repo.config()))
        {
            println!(
                "{{\"status\": \"fast_forwarded\", \"to\": \"{}\"}}",
                target_change_id
            );
        } else if cli.is_some() {
            // Lead with the active thread name (where applicable) so
            // operators don't need to map a worktree path back to a
            // thread mentally. JSON output is unchanged.
            match repo.head_ref()? {
                Head::Attached { thread } => {
                    println!("Fast-forwarded {} to {}", thread, target_change_id.short())
                }
                Head::Detached { .. } => {
                    println!("Fast-forwarded to {}", target_change_id.short())
                }
            }
        }
        return Ok(());
    }

    let commits_to_replay =
        collect_commits_to_rebase(repo, &current_state.change_id, &target_change_id)?;

    if commits_to_replay.is_empty() {
        record_ff_advance(repo, target_thread, &target_change_id)?;
        emit_up_to_date_if_trusted(repo, cli)?;
        return Ok(());
    }

    let rebase_state = RebaseState {
        onto: target_change_id,
        commits_to_replay: commits_to_replay.clone(),
        current_index: 0,
        original_head: current_state.change_id,
        pending_manual_resolution: None,
        pre_conflict_head: None,
        pending_advances: Vec::new(),
        transaction_id: mint_rebase_transaction_id(),
    };

    save_rebase_state(&rebase_state_path, &rebase_state)?;

    if let Some(cli) = cli
        && should_output_json(cli, Some(repo.config()))
    {
        println!(
            "{{\"status\": \"started\", \"commits\": {}}}",
            commits_to_replay.len()
        );
    } else if cli.is_some() {
        println!(
            "Rebasing {} commits onto {}",
            commits_to_replay.len(),
            target_change_id.short()
        );
    }

    if let Some(cli) = cli {
        replay_commits(repo, &rebase_state_path, cli)
    } else {
        replay_commits_silent(repo, &rebase_state_path)
    }
}

fn emit_up_to_date_if_trusted(repo: &Repository, cli: Option<&Cli>) -> Result<()> {
    let Some(cli) = cli else {
        return Ok(());
    };
    let trust = build_repository_verification_state(repo);
    if trust.verified {
        if should_output_json(cli, Some(repo.config())) {
            println!("{{\"status\": \"up_to_date\"}}");
        } else {
            println!("Already up to date");
        }
        return Ok(());
    }

    emit_up_to_date_blocked_by_trust(repo, cli, trust)
}

fn emit_up_to_date_blocked_by_trust(
    repo: &Repository,
    cli: &Cli,
    trust: RepositoryVerificationState,
) -> Result<()> {
    let recommended_action = repository_verification_primary_command(&trust);
    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "status": "blocked",
                "reason": "repository_verification",
                "summary": trust.summary,
                "recommended_action": recommended_action.clone(),
                "recommended_action_template": action_template(&recommended_action),
                "recovery_commands": trust.recovery_commands,
            }))?
        );
    } else {
        println!(
            "Rebase is up to date, but repository verification is blocked: {}",
            trust.summary
        );
        print_next_step(&recommended_action);
    }
    Ok(())
}

fn no_rebase_in_progress_advice(action: &'static str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "no_rebase_in_progress",
        "No rebase in progress",
        "Inspect the current operation state with `heddle status`.",
        "the repository has no persisted Heddle rebase state",
        format!("{action} would need to move worktree and thread state for an active rebase"),
        "repository state was left unchanged",
        "heddle status",
        vec!["heddle status".to_string()],
    )
}

fn rebase_target_required_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "rebase_target_required",
        "Refusing to rebase: target thread required",
        "Inspect available threads with `heddle thread list`, then run `heddle rebase <thread>`.",
        "rebase was requested without a target thread",
        "rebase would need to move the current thread and worktree onto a specific target",
        "repository state was left unchanged",
        "heddle thread list",
        vec![
            "heddle thread list".to_string(),
            "heddle rebase <thread>".to_string(),
        ],
    )
}

fn rebase_target_not_found_advice(target_thread: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "rebase_target_not_found",
        format!("Refusing to rebase: thread '{target_thread}' not found"),
        "Inspect available threads with `heddle thread list`, then rerun rebase with an existing thread.",
        format!("no Heddle thread named '{target_thread}' was found"),
        "rebase would need to move the current thread and worktree onto that target thread",
        "repository state was left unchanged",
        "heddle thread list",
        vec!["heddle thread list".to_string()],
    )
}

fn handle_abort(
    repo: &Repository,
    rebase_state_path: &std::path::Path,
    cli: Option<&Cli>,
) -> Result<()> {
    if !rebase_state_path.exists() {
        return Err(anyhow!(no_rebase_in_progress_advice("abort rebase")));
    }

    // Abort uses the tolerant loader so a crash mid-write to
    // REBASE_STATE (malformed pending_advance entry) still lets the
    // operator rewind via --abort; only `original_head` is required.
    let state = load_rebase_state_for_abort(rebase_state_path)?;
    repo.goto_without_record(&state.original_head)?;

    fs::remove_file(rebase_state_path)?;

    if let Some(cli) = cli
        && should_output_json(cli, Some(repo.config()))
    {
        println!("{{\"status\": \"aborted\"}}");
    } else if cli.is_some() {
        println!("Rebase aborted");
    }

    Ok(())
}

fn handle_continue(
    repo: &Repository,
    rebase_state_path: &std::path::Path,
    cli: Option<&Cli>,
) -> Result<()> {
    if !rebase_state_path.exists() {
        return Err(anyhow!(no_rebase_in_progress_advice("continue rebase")));
    }

    if let Some(cli) = cli {
        replay_commits(repo, rebase_state_path, cli)
    } else {
        replay_commits_silent(repo, rebase_state_path)
    }
}
