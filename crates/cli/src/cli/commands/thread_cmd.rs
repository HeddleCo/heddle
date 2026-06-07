// SPDX-License-Identifier: Apache-2.0
//! Thread command implementation.

use objects::store::ObjectStore;
use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow};
use chrono::Utc;
use objects::{fs_ops::remove_path_recursively, object::ThreadName, store::AgentRegistry};
use refs::Head;
use repo::{
    Repository, Thread, ThreadFreshness, ThreadManager, ThreadMode, ThreadState,
    describe_thread_advice,
};
use serde::Serialize;
use tokio::time::{Duration, sleep};

use super::{
    action_line::print_next,
    advice::RecoveryAdvice,
    git_overlay_health::{PlainGitVerificationProbe, build_plain_git_verification_probe},
    mount_lifecycle,
    operator_core::OperatorCommandOutput,
    operator_loop::primary_next_action,
    thread::{
        cmd_thread_cd, cmd_thread_create, cmd_thread_current, cmd_thread_delete, cmd_thread_list,
        cmd_thread_rename, cmd_thread_show, cmd_thread_switch, find_thread_summary,
        show_thread_summary,
    },
    thread_shaping::{cmd_thread_absorb, cmd_thread_move, cmd_thread_resolve},
    worktree_cmd::helpers::{prepare_worktree_target, write_isolated_checkout},
    worktree_safety::ensure_worktree_clean,
};
use crate::cli::{Cli, ThreadCleanupArgs, ThreadCommands, should_output_json, style};

#[derive(Serialize)]
struct ThreadOutput {
    #[serde(flatten)]
    operator: OperatorCommandOutput,
    thread: Thread,
    changed_path_count: usize,
}

pub(crate) fn thread_manager(repo: &Repository) -> ThreadManager {
    ThreadManager::new(repo.heddle_dir())
}

/// Resolve an optional positional thread identifier to a concrete
/// name. When the user omits the positional, fall back to whichever
/// thread the working checkout is attached to. This is the shared
/// fallback used by `thread show`, `thread refresh`, and
/// `thread captures` — the read-leaning verbs where defaulting to the
/// current thread is unambiguous and high-frequency.
///
/// Resolution is layered:
///   1. A user-supplied positional always wins.
///   2. `repo.current_lane()` — fast attached-HEAD path.
///   3. `current_thread(repo)` — broader resolution that finds the
///      thread record by execution-path lookup. This catches the
///      detached-HEAD-but-inside-a-thread-worktree case, where the
///      checkout is associated with a thread record by
///      `execution_path` even though HEAD itself is detached. PR #69
///      review surfaced this: `thread show/refresh/captures` were
///      hard-failing inside such worktrees.
///
/// The error message is intentionally explicit about both the missing
/// argument and the unavailable fallback so a user in a detached
/// state with no recoverable thread knows exactly what to type next.
pub(crate) fn resolve_thread_name_or_current(
    repo: &Repository,
    name: Option<String>,
    command: &'static str,
    primary_command: impl Into<String>,
) -> Result<String> {
    if let Some(name) = name {
        return Ok(name);
    }
    if let Some(lane) = repo.current_lane()? {
        return Ok(lane);
    }
    if let Some(thread) = current_thread(repo)? {
        return Ok(thread.thread);
    }
    Err(anyhow!(RecoveryAdvice::no_current_thread(
        command,
        Some("<THREAD>"),
        primary_command,
    )))
}

pub(crate) fn current_thread(repo: &Repository) -> Result<Option<Thread>> {
    if let Some(thread) = thread_manager(repo).find_by_execution_root(repo.root())? {
        return Ok(Some(thread));
    }

    let Head::Attached { thread } = repo.head_ref()? else {
        return Ok(None);
    };
    let current_state = repo.refs().get_thread(&thread)?.map(|id| id.short());
    let base_root = current_state
        .as_deref()
        .and_then(|state| repo.resolve_state(state).ok().flatten())
        .and_then(|id| repo.store().get_state(&id).ok().flatten())
        .map(|state| state.tree.short())
        .unwrap_or_default();

    let thread_str = thread.to_string();
    Ok(Some(Thread {
        id: thread_str.clone(),
        thread: thread_str,
        target_thread: None,
        parent_thread: None,
        mode: ThreadMode::Materialized,
        state: ThreadState::Active,
        base_state: current_state.clone().unwrap_or_default(),
        base_root,
        current_state,
        merged_state: None,
        task: None,
        execution_path: repo.root().to_path_buf(),
        materialized_path: None,
        changed_paths: Vec::new(),
        impact_categories: Vec::new(),
        heavy_impact_paths: Vec::new(),
        promotion_suggested: false,
        freshness: ThreadFreshness::Unknown,
        verification_summary: Default::default(),
        confidence_summary: Default::default(),
        integration_policy_result: Default::default(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        ephemeral: None,
        auto: false,
        shared_target_dir: None,
    }))
}

pub(crate) fn load_thread(repo: &Repository, thread_id: &str) -> Result<Thread> {
    match thread_manager(repo).load(thread_id)? {
        Some(thread) => Ok(thread),
        None if repo.refs().get_thread(&ThreadName::new(thread_id))?.is_some() => Err(anyhow!(
            imported_git_ref_not_managed_thread_advice(thread_id)
        )),
        None => Err(anyhow!(thread_not_found_advice(thread_id, "load thread"))),
    }
}

fn render_plain_git_thread_list(cli: &Cli, probe: &PlainGitVerificationProbe) -> Result<()> {
    if should_output_json(cli, None) {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "repository_capability": "plain-git",
                "storage_model": "git",
                "hosted_enabled": false,
                "threads": [],
                "current": null,
                "verification": &probe.trust,
                "recommended_action": &probe.trust.recommended_action,
                "recommended_action_template": &probe.trust.recommended_action_template,
                "recovery_commands": &probe.trust.recovery_commands,
                "recovery_action_templates": &probe.trust.recovery_action_templates,
            }))?
        );
    } else {
        println!("Git repo, Heddle not adopted");
        if let Some(branch) = &probe.git_branch {
            println!("Git branch: {}", style::bold(branch));
        }
        println!("Threads: none");
        println!(
            "Next step: {}",
            style::bold(probe.trust.recommended_action.as_str())
        );
    }
    Ok(())
}

fn render_plain_git_thread_show(
    cli: &Cli,
    probe: &PlainGitVerificationProbe,
    requested: Option<&str>,
) -> Result<()> {
    if should_output_json(cli, None) {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "repository_capability": "plain-git",
                "storage_model": "git",
                "requested_thread": requested,
                "thread": null,
                "verification": &probe.trust,
                "recommended_action": &probe.trust.recommended_action,
                "recommended_action_template": &probe.trust.recommended_action_template,
                "recovery_commands": &probe.trust.recovery_commands,
                "recovery_action_templates": &probe.trust.recovery_action_templates,
            }))?
        );
    } else {
        println!("Git repo, Heddle not adopted");
        if let Some(name) = requested {
            println!("Thread: {}", style::bold(name));
        }
        println!("No Heddle thread is available until this Git repo is adopted.");
        println!(
            "Next step: {}",
            style::bold(probe.trust.recommended_action.as_str())
        );
    }
    Ok(())
}

/// Re-export of `repo::refresh_thread_freshness` so existing CLI
/// callers (`thread.rs`, `merge/mod.rs`, and within this module) keep
/// the `thread_cmd::refresh_thread_freshness` import working without
/// churn. The shared implementation lives in `repo::snapshot_metadata`.
pub(crate) use repo::refresh_thread_freshness;

pub async fn cmd_thread(cli: &Cli, command: ThreadCommands) -> Result<()> {
    let current_dir = std::env::current_dir()?;
    let repo_path = cli.repo.as_ref().unwrap_or(&current_dir);
    match &command {
        ThreadCommands::List(_) => {
            if let Some(probe) = build_plain_git_verification_probe(repo_path)? {
                return render_plain_git_thread_list(cli, &probe);
            }
        }
        ThreadCommands::Show(args) if !args.watch => {
            if let Some(probe) = build_plain_git_verification_probe(repo_path)? {
                return render_plain_git_thread_show(cli, &probe, args.thread.as_deref());
            }
        }
        _ => {}
    }

    let repo = Repository::open(repo_path)?;
    match command {
        ThreadCommands::Create {
            name,
            ephemeral,
            ttl_secs,
        } => cmd_thread_create(cli, &repo, name, ephemeral, ttl_secs),
        ThreadCommands::Switch {
            name,
            print_cd_path,
            force,
        } => cmd_thread_switch(cli, &repo, name, print_cd_path, force),
        ThreadCommands::Current => cmd_thread_current(cli, &repo),
        ThreadCommands::Cd { name } => cmd_thread_cd(&repo, name),
        ThreadCommands::List(args) => cmd_thread_list(cli, &repo, args),
        ThreadCommands::Cleanup(args) => cmd_thread_cleanup(cli, &repo, args),
        ThreadCommands::Show(args) => {
            if args.watch {
                let thread = resolve_thread_name_or_current(
                    &repo,
                    args.thread,
                    "thread show",
                    "heddle thread show <THREAD>",
                )?;
                watch_thread_show(
                    cli,
                    &repo,
                    &thread,
                    args.watch_iterations,
                    args.watch_interval_ms,
                )
                .await
            } else {
                cmd_thread_show(cli, &repo, args.thread)
            }
        }
        ThreadCommands::Captures(args) => {
            let thread = resolve_thread_name_or_current(
                &repo,
                args.thread,
                "thread captures",
                "heddle thread captures <THREAD>",
            )?;
            super::thread::cmd_thread_captures(cli, &repo, &thread, args.limit)
        }
        ThreadCommands::Rename(args) => cmd_thread_rename(cli, &repo, args.old, args.new),
        ThreadCommands::Refresh(args) => {
            let thread = resolve_thread_name_or_current(
                &repo,
                args.thread,
                "thread refresh",
                "heddle thread refresh <THREAD>",
            )?;
            cmd_thread_refresh(cli, &repo, &thread)
        }
        ThreadCommands::Move(args) => {
            cmd_thread_move(cli, args.from, args.to, args.paths, args.message)
        }
        ThreadCommands::Absorb(args) => {
            cmd_thread_absorb(cli, args.thread, args.into, args.message, args.preview)
        }
        ThreadCommands::Resolve(args) => cmd_thread_resolve(cli, args.thread),
        ThreadCommands::Promote(args) => {
            cmd_thread_promote(cli, &repo, &args.thread, args.path, args.force)
        }
        ThreadCommands::Drop(args) => {
            cmd_thread_drop(cli, &repo, &args.thread, args.delete_thread, args.force)
        }
        #[cfg(feature = "client")]
        ThreadCommands::Approve(args) => {
            require_hosted_repo(&repo, "thread approvals")?;
            super::thread_approval::cmd_thread_approve(cli, args).await
        }
        #[cfg(feature = "client")]
        ThreadCommands::Approvals(args) => {
            require_hosted_repo(&repo, "thread approvals")?;
            super::thread_approval::cmd_thread_approvals(cli, args).await
        }
        #[cfg(feature = "client")]
        ThreadCommands::RevokeApproval(args) => {
            require_hosted_repo(&repo, "thread approvals")?;
            super::thread_approval::cmd_thread_revoke_approval(cli, args).await
        }
        #[cfg(feature = "client")]
        ThreadCommands::CheckMerge(args) => {
            require_hosted_repo(&repo, "hosted merge checks")?;
            super::thread_approval::cmd_thread_check_merge(cli, args).await
        }
        #[cfg(not(feature = "client"))]
        ThreadCommands::Approve(_)
        | ThreadCommands::Approvals(_)
        | ThreadCommands::RevokeApproval(_)
        | ThreadCommands::CheckMerge(_) => Err(anyhow!(
            "rebuild cli with --features client to use thread approvals"
        )),
    }
}

#[cfg(feature = "client")]
fn require_hosted_repo(repo: &Repository, feature: &str) -> Result<()> {
    if repo.hosted_enabled() {
        Ok(())
    } else {
        Err(anyhow!(
            "{} require a repository linked to a Heddle hosted upstream. Configure [hosted] in .heddle/config.toml or run this in a hosted-enabled repository.",
            feature
        ))
    }
}

async fn watch_thread_show(
    cli: &Cli,
    repo: &Repository,
    thread_id: &str,
    watch_iterations: Option<usize>,
    watch_interval_ms: Option<u64>,
) -> Result<()> {
    let interval = Duration::from_millis(watch_interval_ms.unwrap_or(1000));
    let mut iterations = 0usize;
    loop {
        let summary = find_thread_summary(repo, thread_id)?
            .ok_or_else(|| anyhow!(thread_not_found_advice(thread_id, "watch thread")))?;
        show_thread_summary(cli, repo, &summary)?;
        iterations += 1;
        if watch_iterations.is_some_and(|limit| iterations >= limit) {
            break;
        }
        sleep(interval).await;
    }
    Ok(())
}

fn cmd_thread_refresh(cli: &Cli, repo: &Repository, thread_id: &str) -> Result<()> {
    let thread = refresh_thread(repo, thread_id, cli)?;
    print_thread_output(
        cli,
        repo,
        thread,
        format!("Refreshed thread '{}'", thread_id),
    )
}

pub(crate) fn refresh_thread(repo: &Repository, thread_id: &str, _cli: &Cli) -> Result<Thread> {
    let manager = thread_manager(repo);
    let mut thread = manager
        .load(thread_id)?
        .ok_or_else(|| anyhow!(thread_not_found_advice(thread_id, "refresh thread")))?;
    let target_thread = thread
        .target_thread
        .clone()
        .ok_or_else(|| anyhow!(RecoveryAdvice::missing_target_thread(thread_id, "refresh thread")))?;

    refresh_thread_freshness(repo, &mut thread)?;
    if thread.freshness == ThreadFreshness::Current {
        return Ok(thread);
    }

    let opened_thread_repo;
    let current_lane = repo.current_lane()?;
    let thread_repo = if thread.execution_path.as_os_str().is_empty() {
        if current_lane.as_deref() == Some(thread.thread.as_str()) {
            repo
        } else {
            return Err(anyhow!(thread_refresh_requires_checkout_advice(
                &thread.thread,
                current_lane.as_deref(),
            )));
        }
    } else {
        opened_thread_repo = Repository::open(&thread.execution_path).map_err(|error| {
            anyhow!(thread_refresh_checkout_unavailable_advice(
                &thread.thread,
                &thread.execution_path,
                error
            ))
        })?;
        &opened_thread_repo
    };
    if let Some(ThreeWayMergeRefresh::Conflicted {
        tree,
        paths,
        ours,
        theirs,
        base,
    }) = preflight_three_way_refresh_conflict(repo, thread_repo, &thread, &target_thread)?
    {
        persist_refresh_conflict_state(thread_repo, tree, ours, theirs, base, paths.clone())?;
        return Err(anyhow!(thread_refresh_conflicted_advice(
            thread_id,
            &paths,
            thread_repo.root(),
        )));
    }
    let rebase_state_path = thread_repo.heddle_dir().join("REBASE_STATE");
    if rebase_state_path.exists() {
        super::rebase::cmd_rebase_silent(thread_repo, None, false, true)?;
    } else {
        super::rebase::cmd_rebase_silent(thread_repo, Some(&target_thread), false, false)?;
    }

    if rebase_state_path.exists() {
        let rebase_state = super::rebase::load_persisted_rebase_state(&rebase_state_path)?;
        let current_state = thread_repo
            .head()?
            .ok_or_else(|| anyhow!("Thread '{}' has no current state after refresh", thread_id))?;
        if rebase_state
            .pending_manual_resolution
            .is_some_and(|pending| pending != current_state)
        {
            fs::remove_file(&rebase_state_path)?;
            thread_repo
                .refs()
                .set_thread(&ThreadName::new(&thread.thread), &current_state)?;
            thread.integration_policy_result.status = Some("manual_resolved".to_string());
            thread.integration_policy_result.reason =
                Some("manual integration resolution captured".to_string());
            thread.integration_policy_result.manual_resolution_state = Some(current_state.short());
        } else {
            // Rebase replays commits one at a time and can flag a
            // conflict on intermediate states even when the *final*
            // tree (target ⊔ thread) merges cleanly — the canonical
            // case is two sibling threads forked from the same base
            // that touch disjoint files. `heddle merge` handles this
            // via a 3-way tree merge; refresh used to fail with a
            // misleading "rebase conflicts" error. Converge the two
            // paths: try the 3-way merge as a fallback. If it's clean
            // we apply it directly; if it actually conflicts we emit
            // a precise blocker pointing at the conflicting paths.
            match try_three_way_merge_refresh(repo, thread_repo, &thread, &target_thread) {
                Err(error) => {
                    restore_refresh_rebase_abort(thread_repo, &rebase_state_path)?;
                    return Err(error);
                }
                Ok(ThreeWayMergeRefresh::Clean { new_state }) => {
                    let _ = fs::remove_file(&rebase_state_path);
                    thread.integration_policy_result.status = Some("manual_resolved".to_string());
                    thread.integration_policy_result.reason =
                        Some("thread refreshed cleanly via 3-way merge fallback".to_string());
                    thread.integration_policy_result.manual_resolution_state =
                        Some(new_state.short());
                }
                Ok(ThreeWayMergeRefresh::Conflicted {
                    tree,
                    paths,
                    ours,
                    theirs,
                    base,
                }) => {
                    restore_refresh_rebase_abort(thread_repo, &rebase_state_path)?;
                    persist_refresh_conflict_state(
                        thread_repo,
                        tree,
                        ours,
                        theirs,
                        base,
                        paths.clone(),
                    )?;
                    return Err(anyhow!(thread_refresh_conflicted_advice(
                        thread_id,
                        &paths,
                        thread_repo.root(),
                    )));
                }
            }
        }
    }
    let current_state = thread_repo
        .head()?
        .ok_or_else(|| anyhow!("Thread '{}' has no current state after refresh", thread_id))?;
    let target_state = repo
        .refs()
        .get_thread(&ThreadName::new(&target_thread))?
        .ok_or_else(|| anyhow!(thread_not_found_advice(&target_thread, "refresh thread")))?;
    let target_state_obj = repo
        .store()
        .get_state(&target_state)?
        .ok_or_else(|| anyhow!("Target state not found"))?;

    thread.base_state = target_state.short();
    thread.base_root = target_state_obj.tree.short();
    thread.current_state = Some(current_state.short());
    thread.integration_policy_result.status = Some("manual_resolved".to_string());
    thread.integration_policy_result.reason =
        Some("thread refreshed cleanly onto target".to_string());
    thread.integration_policy_result.manual_resolution_state = Some(current_state.short());
    thread.updated_at = Utc::now();
    thread.freshness = ThreadFreshness::Current;
    manager.save(&thread)?;
    Ok(thread)
}

fn restore_refresh_rebase_abort(repo: &Repository, rebase_state_path: &Path) -> Result<()> {
    let rebase_state = super::rebase::load_persisted_rebase_state(rebase_state_path)?;
    let head_before = repo.head_ref()?;
    repo.goto_without_record_discard_local(&rebase_state.original_head)?;
    if let Head::Attached { thread } = head_before {
        repo.refs()
            .set_thread(&thread, &rebase_state.original_head)?;
        repo.refs().write_head(&Head::Attached {
            thread: thread.clone(),
        })?;
        let manager = thread_manager(repo);
        if let Some(mut metadata) = manager.find_by_thread(&thread)? {
            let state = repo
                .store()
                .get_state(&rebase_state.original_head)?
                .ok_or_else(|| anyhow!("Original rebase state not found"))?;
            repo::update_thread_state_from_state(&mut metadata, &state);
            refresh_thread_freshness(repo, &mut metadata)?;
            metadata.updated_at = Utc::now();
            manager.save(&metadata)?;
        }
    }
    if rebase_state_path.exists() {
        fs::remove_file(rebase_state_path)?;
    }
    Ok(())
}

fn persist_refresh_conflict_state(
    thread_repo: &Repository,
    tree: objects::object::Tree,
    ours: objects::object::ChangeId,
    theirs: objects::object::ChangeId,
    base: Option<objects::object::ChangeId>,
    paths: Vec<String>,
) -> Result<()> {
    super::merge::apply_merged_tree_external(thread_repo, &tree)?;
    ensure_refresh_conflict_markers_materialized(thread_repo, &ours, &theirs, &paths)?;
    thread_repo
        .merge_state_manager()
        .start(ours, theirs, base, paths)?;
    Ok(())
}

fn ensure_refresh_conflict_markers_materialized(
    repo: &Repository,
    ours: &objects::object::ChangeId,
    theirs: &objects::object::ChangeId,
    paths: &[String],
) -> Result<()> {
    let ours_tree = tree_for_state(repo, ours)?;
    let theirs_tree = tree_for_state(repo, theirs)?;
    for path in paths {
        let full_path = repo.root().join(path);
        let existing = fs::read(&full_path).unwrap_or_default();
        if contains_conflict_marker_bytes(&existing) {
            continue;
        }
        let ours_content = conflict_side_content(repo, &ours_tree, path)?;
        let theirs_content = conflict_side_content(repo, &theirs_tree, path)?;
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            full_path,
            format_refresh_conflict_markers(&ours_content, &theirs_content),
        )?;
    }
    Ok(())
}

fn tree_for_state(
    repo: &Repository,
    state_id: &objects::object::ChangeId,
) -> Result<objects::object::Tree> {
    let state = repo
        .store()
        .get_state(state_id)?
        .ok_or_else(|| anyhow!("State '{}' not found", state_id.short()))?;
    repo.require_tree(&state.tree).map_err(Into::into)
}

fn conflict_side_content(
    repo: &Repository,
    tree: &objects::object::Tree,
    path: &str,
) -> Result<Vec<u8>> {
    let Some(entry) = tree.get(path) else {
        return Ok(Vec::new());
    };
    if entry.entry_type != objects::object::EntryType::Blob
        && entry.entry_type != objects::object::EntryType::Symlink
    {
        return Ok(Vec::new());
    }
    Ok(repo.require_blob(&entry.hash)?.content().to_vec())
}

fn contains_conflict_marker_bytes(content: &[u8]) -> bool {
    content
        .windows("<<<<<<<".len())
        .any(|window| window == b"<<<<<<<")
        && content
            .windows("=======".len())
            .any(|window| window == b"=======")
        && content
            .windows(">>>>>>>".len())
            .any(|window| window == b">>>>>>>")
}

fn format_refresh_conflict_markers(ours: &[u8], theirs: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"<<<<<<< CURRENT\n");
    out.extend_from_slice(ours);
    if !ours.ends_with(b"\n") {
        out.push(b'\n');
    }
    out.extend_from_slice(b"=======\n");
    out.extend_from_slice(theirs);
    if !theirs.ends_with(b"\n") {
        out.push(b'\n');
    }
    out.extend_from_slice(b">>>>>>> INCOMING\n");
    out
}

fn thread_refresh_conflicted_advice(
    thread_id: &str,
    paths: &[String],
    conflict_repo: &Path,
) -> RecoveryAdvice {
    let path_summary = if paths.is_empty() {
        "merge conflict detected".to_string()
    } else {
        paths
            .iter()
            .take(12)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    };
    let overflow = paths.len().saturating_sub(12);
    let unsafe_condition = if overflow == 0 {
        format!("{} conflicting path(s): {}", paths.len(), path_summary)
    } else {
        format!(
            "{} conflicting path(s): {}, and {} more",
            paths.len(),
            path_summary,
            overflow
        )
    };
    let repo_arg = quote_recommended_action_arg(&conflict_repo.display().to_string());
    let conflict_list_command = format!("heddle --repo {repo_arg} conflict list");
    let first_conflict_command = paths.first().map(|path| {
        format!(
            "heddle --repo {repo_arg} conflict show {}",
            quote_recommended_action_arg(path)
        )
    });
    let resolve_command = paths.first().map(|path| {
        format!(
            "heddle --repo {repo_arg} resolve {}",
            quote_recommended_action_arg(path)
        )
    });
    let continue_command = format!("heddle --repo {repo_arg} continue");
    let preview_command = super::thread_landing::merge_preview_command(thread_id);

    let mut recovery_commands = vec![conflict_list_command.clone()];
    if let Some(show) = first_conflict_command.clone() {
        recovery_commands.push(show);
    }
    if let Some(resolve) = resolve_command.clone() {
        recovery_commands.push(resolve);
    }
    recovery_commands.push(continue_command.clone());
    recovery_commands.push(preview_command.clone());

    RecoveryAdvice::safety_refusal(
        "thread_refresh_conflicted",
        format!(
            "Thread '{thread_id}' could not be refreshed cleanly: {} conflicting path(s) ({path_summary})",
            paths.len(),
        ),
        format!(
            "Refresh wrote conflict markers and merge state in {}. Inspect them with `{conflict_list_command}`, resolve the files, then run `{continue_command}`.",
            conflict_repo.display()
        ),
        unsafe_condition,
        "refresh would need resolved file contents before advancing the thread ref",
        "the thread ref was left unchanged; conflict markers and merge state were written to the thread checkout for inspection",
        conflict_list_command,
        recovery_commands,
    )
}

fn quote_recommended_action_arg(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b'+'))
    {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn thread_refresh_requires_checkout_advice(
    thread_id: &str,
    current: Option<&str>,
) -> RecoveryAdvice {
    let switch_command = format!("heddle switch {thread_id}");
    let retry_command = format!("heddle thread refresh {thread_id}");
    let current_summary = current
        .map(|name| format!("current checkout is attached to '{name}'"))
        .unwrap_or_else(|| "current checkout is not attached to a thread".to_string());
    RecoveryAdvice::safety_refusal(
        "thread_refresh_requires_checkout",
        format!("Thread '{thread_id}' must be the current checkout before refresh"),
        format!("Run `{switch_command}`, then `{retry_command}`."),
        format!("thread '{thread_id}' has no dedicated checkout; {current_summary}"),
        "refresh would update the requested thread checkout and cannot safely do that from another branch-like checkout",
        "thread refs, checkout files, and Heddle metadata were left unchanged",
        switch_command.clone(),
        vec![switch_command, retry_command],
    )
}

fn thread_refresh_checkout_unavailable_advice(
    thread_id: &str,
    path: &Path,
    error: impl std::fmt::Display,
) -> RecoveryAdvice {
    let inspect_command = format!("heddle thread show {thread_id}");
    let switch_command = format!("heddle switch {thread_id}");
    RecoveryAdvice::safety_refusal(
        "thread_refresh_checkout_unavailable",
        format!("Thread '{thread_id}' checkout could not be opened"),
        format!(
            "Inspect the recorded checkout with `{inspect_command}`. If this is a branch-like thread, run `{switch_command}` from the main checkout before refreshing."
        ),
        format!(
            "recorded checkout path '{}' is unavailable: {error}",
            path.display()
        ),
        "refresh needs a readable Heddle checkout before it can rebase or merge the thread state",
        "thread refs, checkout files, and Heddle metadata were left unchanged",
        inspect_command.clone(),
        vec![inspect_command, switch_command],
    )
}

pub(crate) fn thread_not_found_advice(thread_id: &str, action: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "thread_not_found",
        format!("Thread '{thread_id}' not found"),
        "Inspect available threads with `heddle thread list`, then retry with an existing thread.",
        format!("{action} was requested for missing thread '{thread_id}'"),
        "the command cannot safely change or remove thread metadata that does not exist",
        "no thread refs, checkout directories, mounts, or agent reservations were changed",
        "heddle thread list",
        vec!["heddle thread list".to_string()],
    )
}

fn imported_git_ref_not_managed_thread_advice(thread_id: &str) -> RecoveryAdvice {
    let reconcile_preview =
        super::git_overlay_health::canonical_bridge_reconcile_ref_preview_command(None, thread_id);
    RecoveryAdvice::safety_refusal(
        "imported_git_ref_not_managed_thread",
        format!("'{thread_id}' is an imported Git ref, not a managed Heddle thread"),
        format!(
            "Preview Git/Heddle reconciliation with `{reconcile_preview}`. Use managed threads for `ready` and `land`."
        ),
        format!("thread ref '{thread_id}' exists, but no managed thread metadata exists for it"),
        "ready/land require managed thread metadata and explicit integration authority; treating an imported Git ref as landable would be ambiguous",
        "thread refs, Git refs, checkout files, and thread metadata were left unchanged",
        reconcile_preview.clone(),
        vec![reconcile_preview, "heddle thread list".to_string()],
    )
}

fn current_thread_drop_advice(repo: &Repository, thread_id: &str) -> RecoveryAdvice {
    let (primary, recovery, hint) = super::thread::current_thread_drop_recovery(
        repo,
        thread_id,
        super::thread::DropMode::Drop,
    );
    RecoveryAdvice::safety_refusal(
        "current_thread_not_droppable",
        format!("Thread '{thread_id}' is the current checkout thread and cannot be dropped"),
        hint,
        format!("drop thread was requested for the attached checkout thread '{thread_id}'"),
        "dropping the current checkout would remove the thread that owns this working tree",
        "no thread refs, checkout directories, mounts, or agent reservations were changed",
        primary,
        recovery,
    )
}

fn thread_cleanup_mode_required_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "thread_cleanup_mode_required",
        "heddle thread cleanup requires at least one mode flag: pass --merged to sweep merged threads, --auto --older-than <duration> to sweep stale auto-threads, or both.",
        "Choose `heddle thread cleanup --merged --dry-run` or `heddle thread cleanup --auto --older-than 7d --dry-run` first.",
        "thread cleanup was invoked without a mode flag",
        "cleanup without an explicit mode could remove threads the user did not mean to sweep",
        "no thread metadata, checkout directories, mounts, or reservations were changed",
        "heddle thread cleanup --merged --dry-run",
        vec![
            "heddle thread cleanup --merged --dry-run".to_string(),
            "heddle thread cleanup --auto --older-than 7d --dry-run".to_string(),
        ],
    )
}

fn thread_cleanup_auto_required_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "thread_cleanup_auto_required",
        "--older-than only applies with --auto; pass --auto to sweep stale auto-threads.",
        "Run `heddle thread cleanup --auto --older-than 7d --dry-run`, or remove `--older-than` when sweeping merged threads.",
        "`--older-than` was provided without `--auto`",
        "cleanup cannot apply age-based filtering unless it is restricted to auto-threads",
        "no thread metadata, checkout directories, mounts, or reservations were changed",
        "heddle thread cleanup --auto --older-than 7d --dry-run",
        vec!["heddle thread cleanup --auto --older-than 7d --dry-run".to_string()],
    )
}

fn thread_cleanup_older_than_required_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "thread_cleanup_older_than_required",
        "--auto requires --older-than <duration> (e.g. --older-than 7d) so the sweep does not drop a thread you just created.",
        "Run `heddle thread cleanup --auto --older-than 7d --dry-run` before sweeping stale auto-threads.",
        "`--auto` was provided without an age threshold",
        "cleanup cannot distinguish stale auto-threads from freshly created auto-threads",
        "no thread metadata, checkout directories, mounts, or reservations were changed",
        "heddle thread cleanup --auto --older-than 7d --dry-run",
        vec!["heddle thread cleanup --auto --older-than 7d --dry-run".to_string()],
    )
}

fn thread_cleanup_duration_advice(spec: &str, detail: impl Into<String>) -> RecoveryAdvice {
    let detail = detail.into();
    RecoveryAdvice::safety_refusal(
        "thread_cleanup_invalid_duration",
        detail.clone(),
        "Use a non-negative duration like `7d`, `24h`, `30m`, or bare seconds.",
        format!("invalid --older-than duration '{spec}': {detail}"),
        "cleanup cannot decide which auto-threads are stale without a valid duration",
        "no thread metadata, checkout directories, mounts, or reservations were changed",
        "heddle thread cleanup --auto --older-than 7d --dry-run",
        vec!["heddle thread cleanup --auto --older-than 7d --dry-run".to_string()],
    )
}

/// Outcome of the 3-way merge fallback used when commit-by-commit
/// rebase replay fails but the final trees may still merge cleanly.
enum ThreeWayMergeRefresh {
    Clean {
        new_state: objects::object::ChangeId,
    },
    Conflicted {
        tree: objects::object::Tree,
        paths: Vec<String>,
        ours: objects::object::ChangeId,
        theirs: objects::object::ChangeId,
        base: Option<objects::object::ChangeId>,
    },
}

fn preflight_three_way_refresh_conflict(
    parent_repo: &Repository,
    thread_repo: &Repository,
    thread: &Thread,
    target_thread_name: &str,
) -> Result<Option<ThreeWayMergeRefresh>> {
    use super::merge::{
        ConflictLabels, MergeStrategy, ThreeWayMergeOutcome, try_three_way_merge_between_tips,
    };

    let target_tip = parent_repo
        .refs()
        .get_thread(&ThreadName::new(target_thread_name))?
        .ok_or_else(|| {
            anyhow!(thread_not_found_advice(
                target_thread_name,
                "preflight thread refresh",
            ))
        })?;
    let thread_tip = parent_repo
        .refs()
        .get_thread(&ThreadName::new(&thread.thread))?
        .ok_or_else(|| {
            anyhow!(
                "managed thread '{}' is missing its ref during refresh preflight",
                thread.thread
            )
        })?;
    let current_label = format!("CURRENT ({})", thread.thread);
    let incoming_label = format!("INCOMING ({})", target_thread_name);

    match try_three_way_merge_between_tips(
        parent_repo,
        &thread_tip,
        &target_tip,
        ConflictLabels {
            current: &current_label,
            incoming: &incoming_label,
            strategy: MergeStrategy::Semantic,
        },
    )? {
        ThreeWayMergeOutcome::Conflicted { tree, paths, base } => {
            let _ = thread_repo;
            Ok(Some(ThreeWayMergeRefresh::Conflicted {
                tree,
                paths,
                ours: thread_tip,
                theirs: target_tip,
                base: Some(base),
            }))
        }
        ThreeWayMergeOutcome::Clean { .. }
        | ThreeWayMergeOutcome::AlreadyIntegrated { .. }
        | ThreeWayMergeOutcome::FastForward { .. } => Ok(None),
    }
}

/// Try to refresh a thread by performing a 3-way merge between the
/// thread tip and the target tip (instead of replaying commits). This
/// is the same algorithm `heddle merge` uses, so refresh and merge
/// agree on whether two thread tips can be combined cleanly.
///
/// On success, the thread's worktree is updated with the merged tree
/// and a new merge state is snapshotted as the new thread tip; the
/// caller advances `thread.current_state` based on the returned id.
///
/// On conflict, returns the partial merge tree plus conflict metadata so
/// the caller can persist durable conflict state before reporting the
/// blocker.
fn try_three_way_merge_refresh(
    parent_repo: &Repository,
    thread_repo: &Repository,
    thread: &Thread,
    target_thread_name: &str,
) -> Result<ThreeWayMergeRefresh> {
    use objects::object::Attribution;

    use super::merge::{
        ConflictLabels, MergeStrategy, ThreeWayMergeOutcome, try_three_way_merge_between_tips,
    };

    let target_tip = parent_repo
        .refs()
        .get_thread(&ThreadName::new(target_thread_name))?
        .ok_or_else(|| {
            anyhow!(thread_not_found_advice(
                target_thread_name,
                "refresh thread",
            ))
        })?;
    let thread_tip = parent_repo
        .refs()
        .get_thread(&ThreadName::new(&thread.thread))?
        .ok_or_else(|| {
            anyhow!(
                "managed thread '{}' is missing its ref during refresh",
                thread.thread
            )
        })?;

    let current_label = format!("CURRENT ({})", thread.thread);
    let incoming_label = format!("INCOMING ({})", target_thread_name);
    // Thread refresh is the canonical "I have local work, let me pull in
    // upstream changes" workflow — exactly the case where structural-overlap
    // merges benefit from function-level resolution. Route through the
    // AST-aware driver from heddle#68 (PR #114, commit 79104f9); the driver
    // itself falls back to text_hunk_merge on unknown / unparseable files,
    // and when the `semantic` feature is compiled out the variant collapses
    // to the same HunkOnly path the historical code took.
    let outcome = try_three_way_merge_between_tips(
        parent_repo,
        &thread_tip,
        &target_tip,
        ConflictLabels {
            current: &current_label,
            incoming: &incoming_label,
            strategy: MergeStrategy::Semantic,
        },
    )?;

    match outcome {
        ThreeWayMergeOutcome::AlreadyIntegrated { target } => {
            // Thread already contains target. Refresh is a no-op
            // beyond the metadata bookkeeping the caller does.
            Ok(ThreeWayMergeRefresh::Clean { new_state: target })
        }
        ThreeWayMergeOutcome::FastForward { target } => {
            // Thread is strictly behind target — fast-forward the
            // thread ref. We do this against the parent repo so the
            // ref move is visible to the caller's bookkeeping.
            parent_repo.refs().set_thread(&ThreadName::new(&thread.thread), &target)?;
            // Materialize the target tree to the thread's worktree.
            // Without this, HEAD metadata advances while the files on
            // disk stay stale and subsequent operations run against a
            // stale checkout.
            let target_state = parent_repo
                .store()
                .get_state(&target)?
                .ok_or_else(|| anyhow!("Target state not found during fast-forward refresh"))?;
            let target_tree = parent_repo
                .store()
                .get_tree(&target_state.tree)?
                .ok_or_else(|| anyhow!("Target tree not found during fast-forward refresh"))?;
            super::merge::apply_merged_tree_external(thread_repo, &target_tree)?;
            Ok(ThreeWayMergeRefresh::Clean { new_state: target })
        }
        ThreeWayMergeOutcome::Clean { tree } => {
            // Apply the merged tree to the thread's worktree, then
            // capture a merge state and advance the thread ref. We
            // operate against `thread_repo` because that's where the
            // thread's worktree lives.
            super::merge::apply_merged_tree_external(thread_repo, &tree)?;
            let attribution = Attribution::human(thread_repo.get_principal()?);
            let new_state = thread_repo.snapshot_merge_with_attribution(
                &target_tip,
                Some(format!(
                    "Refresh thread '{}' onto '{}'",
                    thread.thread, target_thread_name
                )),
                None,
                attribution,
                None,
                false,
            )?;
            parent_repo
                .refs()
                .set_thread(&ThreadName::new(&thread.thread), &new_state.change_id)?;
            Ok(ThreeWayMergeRefresh::Clean {
                new_state: new_state.change_id,
            })
        }
        ThreeWayMergeOutcome::Conflicted { tree, paths, base } => {
            Ok(ThreeWayMergeRefresh::Conflicted {
                tree,
                paths,
                ours: thread_tip,
                theirs: target_tip,
                base: Some(base),
            })
        }
    }
}

fn cmd_thread_promote(
    cli: &Cli,
    repo: &Repository,
    thread_id: &str,
    path: Option<PathBuf>,
    force: bool,
) -> Result<()> {
    let manager = thread_manager(repo);
    let mut thread = manager
        .load(thread_id)?
        .ok_or_else(|| anyhow!(thread_not_found_advice(thread_id, "promote thread")))?;
    let state_id = repo.refs().get_thread(&ThreadName::new(&thread.thread))?.ok_or_else(|| {
        anyhow!(
            "managed thread '{}' is missing its ref during promote",
            thread.thread
        )
    })?;
    if !force {
        if thread.execution_path.exists()
            && thread.execution_path != *repo.root()
            && thread.execution_path.join(".heddle").exists()
        {
            match Repository::open(&thread.execution_path) {
                Ok(guard_repo) => ensure_worktree_clean(&guard_repo, "promote thread")?,
                Err(_) => ensure_worktree_clean(repo, "promote thread")?,
            }
        } else {
            ensure_worktree_clean(repo, "promote thread")?;
        }
    }
    // Promoting away from a virtual mount: tear it down first so
    // the mount point can be safely abandoned by the new
    // materialized checkout under a different path. The mount may
    // be owned by either the in-process registry (for `--no-daemon`
    // mounts) or the long-lived daemon (for `--daemon` mounts —
    // which is the default for Virtualized threads). We need to ask
    // both: a daemon-owned mount left behind here would orphan a
    // live FUSE session because the post-promote `Materialized`
    // mode causes any future `thread drop` to skip the daemon
    // unmount branch. Both calls are best-effort, mirroring
    // `cmd_thread_drop`.
    if matches!(thread.mode, ThreadMode::Virtualized) {
        mount_lifecycle::unmount_thread_if_mounted(thread_id);
        if let Err(error) =
            crate::cli::commands::daemon_client::unmount_via_daemon(repo.root(), thread_id)
        {
            tracing::warn!(
                thread = thread_id,
                %error,
                "daemon unmount RPC failed during promote; continuing"
            );
        }
    }
    let path = path.unwrap_or_else(|| default_materialized_thread_path(repo, thread_id));
    let abs_path = prepare_worktree_target(repo, &path)?.path;
    write_isolated_checkout(repo, &abs_path, &state_id, Some(&thread.thread))?;

    thread.mode = ThreadMode::Solid;
    thread.state = ThreadState::Promoted;
    thread.materialized_path = Some(abs_path.clone());
    thread.updated_at = Utc::now();
    manager.save(&thread)?;

    print_thread_output(
        cli,
        repo,
        thread,
        format!(
            "Promoted thread '{}' to '{}'",
            thread_id,
            abs_path.display()
        ),
    )
}

pub(crate) fn cmd_thread_drop(
    cli: &Cli,
    repo: &Repository,
    thread_id: &str,
    delete_thread: bool,
    force: bool,
) -> Result<()> {
    let outcome = drop_thread_silent(repo, thread_id, delete_thread, force)?;
    match outcome {
        DropOutcome::Dropped(thread) => print_thread_output(
            cli,
            repo,
            *thread,
            format!("Dropped thread '{}'", thread_id),
        ),
        DropOutcome::Deleted => cmd_thread_delete(cli, repo, thread_id.to_string()),
    }
}

/// Result of a silent thread-drop. Carries the abandoned `Thread`
/// record so callers can render their own output, or signals that
/// the record was missing and a delete-thread request was honored.
///
/// The `Thread` is boxed because clippy flags the size disparity
/// between the two variants — the typed metadata is ~500 bytes and
/// `Deleted` is empty. Boxing keeps the enum small at the call site.
pub(crate) enum DropOutcome {
    Dropped(Box<Thread>),
    Deleted,
}

/// Tear down an active thread without printing. Used by `cmd_thread_drop`
/// (which then prints) and by `heddle try` (which embeds the drop
/// inside its own output). The tear-down sequence is identical to the
/// public verb: unmount → remove checkout → mark Abandoned → strip
/// agent registry entries → optionally delete the ref.
pub(crate) fn drop_thread_silent(
    repo: &Repository,
    thread_id: &str,
    delete_thread: bool,
    force: bool,
) -> Result<DropOutcome> {
    let manager = thread_manager(repo);
    let Some(mut thread) = manager.load(thread_id)? else {
        if !delete_thread && repo.current_lane()?.as_deref() == Some(thread_id) {
            return Err(anyhow!(current_thread_drop_advice(repo, thread_id)));
        }
        if delete_thread {
            return Ok(DropOutcome::Deleted);
        }
        return Err(anyhow!(thread_not_found_advice(thread_id, "drop thread")));
    };
    if !force {
        if thread.execution_path.exists()
            && thread.execution_path != *repo.root()
            && thread.execution_path.join(".heddle").exists()
        {
            match Repository::open(&thread.execution_path) {
                Ok(guard_repo) => ensure_worktree_clean(&guard_repo, "drop thread")?,
                Err(_) => ensure_worktree_clean(repo, "drop thread")?,
            }
        } else {
            ensure_worktree_clean(repo, "drop thread")?;
        }
    }

    // Virtualized threads need the FUSE mount torn down *before* the
    // execution-path directory is removed, otherwise the rmdir hits
    // EBUSY against a live mount. `unmount_thread_if_mounted` is a
    // best-effort no-op for any thread we never registered (which
    // includes every Virtualized thread started by a previous CLI
    // invocation, since the registry is process-local — see the
    // TODO in `mount_lifecycle::spawn_mount_for_thread`). On
    // non-Linux/no-feature builds the function is a stub that
    // always returns `false`.
    if matches!(thread.mode, ThreadMode::Virtualized) {
        // Two paths here: the in-process registry (for non-`--daemon`
        // mounts) and the long-lived daemon (for `--daemon` mounts
        // that may have been established by a previous CLI invocation).
        // Both are best-effort: in-process is a no-op when the
        // registry has no entry; daemon is a no-op when no daemon is
        // running. Order doesn't matter — the two paths are disjoint
        // by construction (a mount is owned by exactly one of them).
        mount_lifecycle::unmount_thread_if_mounted(thread_id);
        if let Err(error) =
            crate::cli::commands::daemon_client::unmount_via_daemon(repo.root(), thread_id)
        {
            tracing::warn!(thread = thread_id, %error, "daemon unmount RPC failed; continuing with drop");
        }
    }
    if thread.execution_path.exists() {
        // For Virtualized threads `execution_path` is the mount
        // point. After unmount it should be an empty directory we
        // can safely rmdir; if the unmount failed `remove_path_recursively`
        // will also fail and the user gets a clear error.
        remove_path_recursively(&thread.execution_path)?;
    }
    // Drop the manifest sidecar last — it has no on-disk dependencies
    // and a leftover would surface as a phantom entry in
    // `heddle status` and `heddle daemon status`. Best-effort: a
    // missing dir reports `false` rather than erroring, an inaccessible
    // dir bubbles up the io error so the drop reports the actual
    // problem instead of silently leaving inventory inconsistent.
    repo::thread_manifest::remove_thread_manifest_dir(repo.heddle_dir(), &thread.thread)?;
    thread.state = ThreadState::Abandoned;
    thread.updated_at = Utc::now();
    manager.save(&thread)?;
    let registry = AgentRegistry::new(repo.heddle_dir());
    for entry in registry.list()? {
        if entry.thread == thread.thread || entry.thread_id.as_deref() == Some(thread_id) {
            registry.delete(&entry.session_id)?;
        }
    }
    let tn = ThreadName::new(&thread.thread);
    if delete_thread && repo.refs().get_thread(&tn)?.is_some() {
        repo.refs().delete_thread(&tn)?;
    }
    Ok(DropOutcome::Dropped(Box::new(thread)))
}

fn print_thread_output(
    cli: &Cli,
    repo: &Repository,
    mut thread: Thread,
    message: String,
) -> Result<()> {
    refresh_thread_freshness(repo, &mut thread)?;
    let mut advice = describe_thread_advice(&thread, false, 0, false);
    if matches!(thread.state, ThreadState::Abandoned) {
        advice.blockers.clear();
        advice.recommended_action.clear();
    }
    let operation = repo.operation_status()?;
    let remote_tracking = repo.git_remote_tracking_status()?;
    let import_hint = repo.git_overlay_import_hint()?;
    let recommended_action = primary_next_action(
        operation.as_ref(),
        remote_tracking.as_ref(),
        import_hint.as_ref(),
        Some(&advice.recommended_action),
    );
    let recommended_action = (!recommended_action.trim().is_empty()).then_some(recommended_action);
    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{}",
            serde_json::to_string(&ThreadOutput {
                operator: OperatorCommandOutput {
                    status: "completed".to_string(),
                    action: "thread".to_string(),
                    message,
                    blockers: advice.blockers.clone(),
                    warnings: Vec::new(),
                    next_action: recommended_action.clone(),
                    recommended_action: recommended_action.clone(),
                },
                changed_path_count: thread.changed_paths.len(),
                thread,
            })?
        );
    } else {
        println!("{}", message);
        if !advice.blockers.is_empty() {
            println!("Blockers:");
            for blocker in &advice.blockers {
                println!("  - {}", blocker);
            }
        }
        if let Some(recommended_action) = &recommended_action {
            print_next(recommended_action);
        }
    }
    Ok(())
}

fn default_materialized_thread_path(repo: &Repository, thread_id: &str) -> PathBuf {
    // Promote to a solid checkout under the same Heddle-managed
    // `.heddle/threads/<name>/root` layout the `start` defaults use, so a
    // promoted thread stays reachable from a repo-scoped sandbox and out
    // of the parent repo's status. The `root/` leaf keeps the per-thread
    // `manifest.toml` sidecar a sibling of the checkout, not inside it.
    // See `thread::default_threads_root`.
    super::thread::default_threads_root(repo)
        .join(thread_id.replace('/', "-"))
        .join("root")
}

// --- thread cleanup -------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct ThreadCleanupOutput {
    #[serde(flatten)]
    operator: OperatorCommandOutput,
    /// Whether the run was a dry run (no on-disk changes performed).
    dry_run: bool,
    /// Threads dropped (or that would be dropped, in dry-run) because
    /// their lifecycle state is `merged`.
    merged: Vec<DroppedThread>,
    /// Threads dropped (or that would be dropped, in dry-run) because
    /// they are auto-created and stale per `--older-than`.
    auto: Vec<DroppedThread>,
    /// Total bytes reclaimed from removing thread checkouts. Always
    /// `0` in dry-run mode — see `would_reclaim_bytes` for the
    /// estimate. This split prevents automation that reads
    /// `reclaimed_bytes` from misinterpreting a dry-run estimate as
    /// actually-reclaimed bytes.
    reclaimed_bytes: u64,
    /// Estimated bytes that *would* be reclaimed if the run were
    /// applied. Mirrors `reclaimed_bytes` in non-dry-run mode and
    /// surfaces the projected reclaim in dry-run mode.
    would_reclaim_bytes: u64,
    /// Threads that matched the cleanup criteria but were skipped
    /// (e.g. the active thread the user is currently inside). Empty
    /// in the common case.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    skipped: Vec<SkippedThread>,
}

#[derive(Debug, Clone, Serialize)]
struct DroppedThread {
    thread: String,
    id: String,
    reason: &'static str,
    age_seconds: i64,
    /// Bytes the thread checkout occupied on disk before removal.
    /// `0` when no execution path existed (e.g. lightweight thread
    /// with the checkout already pruned).
    bytes: u64,
    execution_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SkippedThread {
    thread: String,
    id: String,
    /// Stable reason code so automation can branch on it. Currently
    /// only `active` is emitted.
    reason: &'static str,
    /// Human-readable note explaining the skip — e.g. why dropping
    /// the active thread would leave the user in a deleted directory.
    note: String,
}

/// Parse a duration spec accepted by `--older-than`. Recognizes the
/// suffixes `s`, `m`, `h`, `d`, `w` (lowercase) and a bare integer
/// (interpreted as seconds). The grammar is intentionally tiny — we
/// don't need humantime here and avoiding the dep keeps the CLI's
/// surface area small.
fn parse_duration(spec: &str) -> Result<chrono::Duration> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err(anyhow!(thread_cleanup_duration_advice(
            spec,
            "duration is empty"
        )));
    }
    // Bare integer => seconds. Use the fallible constructor so absurdly
    // large inputs return a user-facing error instead of panicking
    // inside chrono — matches the suffixed paths below.
    if let Ok(seconds) = trimmed.parse::<i64>() {
        if seconds < 0 {
            return Err(anyhow!(thread_cleanup_duration_advice(
                spec,
                "duration must be non-negative"
            )));
        }
        return chrono::Duration::try_seconds(seconds).ok_or_else(|| {
            anyhow!(thread_cleanup_duration_advice(
                spec,
                format!("duration overflow: '{spec}' exceeds chrono's range")
            ))
        });
    }
    let (num_part, unit) = trimmed.split_at(trimmed.len() - 1);
    let value: i64 = num_part.parse().map_err(|_| {
        anyhow!(thread_cleanup_duration_advice(
            spec,
            format!(
                "could not parse duration '{spec}' - expected a non-negative integer with optional suffix s/m/h/d/w (e.g. 7d, 24h)"
            )
        ))
    })?;
    if value < 0 {
        return Err(anyhow!(thread_cleanup_duration_advice(
            spec,
            "duration must be non-negative"
        )));
    }
    // Use the `try_*` constructors so absurdly large inputs (e.g.
    // `9223372036854775807w`) return an error instead of panicking
    // inside chrono's overflow-checked multiplication.
    let duration = match unit {
        "s" => chrono::Duration::try_seconds(value),
        "m" => chrono::Duration::try_minutes(value),
        "h" => chrono::Duration::try_hours(value),
        "d" => chrono::Duration::try_days(value),
        "w" => chrono::Duration::try_weeks(value),
        other => {
            return Err(anyhow!(thread_cleanup_duration_advice(
                spec,
                format!(
                    "unknown duration unit '{other}' in '{spec}' - expected one of s, m, h, d, w"
                )
            )));
        }
    };
    duration.ok_or_else(|| {
        anyhow!(thread_cleanup_duration_advice(
            spec,
            format!("duration overflow: {spec}")
        ))
    })
}

/// Walk a directory tree summing the apparent file sizes. Returns 0
/// for a non-existent path; logs (best-effort) but never errors out
/// of the cleanup loop on partial traversal failures.
fn directory_size(path: &std::path::Path) -> u64 {
    if !path.exists() {
        return 0;
    }
    let mut total: u64 = 0;
    let walker = match fs::read_dir(path) {
        Ok(walker) => walker,
        Err(err) => {
            tracing::debug!(path = %path.display(), %err, "directory_size: read_dir failed");
            return 0;
        }
    };
    for entry in walker.flatten() {
        let entry_path = entry.path();
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => total += directory_size(&entry_path),
            Ok(ft) if ft.is_file() => {
                if let Ok(meta) = entry.metadata() {
                    total = total.saturating_add(meta.len());
                }
            }
            _ => {}
        }
    }
    total
}

/// Render a byte count in IEC-ish units (KB/MB/GB/TB) so the human
/// summary doesn't dump 28000000000 bytes inline.
fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    const TB: f64 = GB * 1024.0;
    let b = bytes as f64;
    if b >= TB {
        format!("{:.1} TB", b / TB)
    } else if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

fn cmd_thread_cleanup(cli: &Cli, repo: &Repository, args: ThreadCleanupArgs) -> Result<()> {
    if !args.merged && !args.auto {
        return Err(anyhow!(thread_cleanup_mode_required_advice()));
    }
    if args.older_than.is_some() && !args.auto {
        return Err(anyhow!(thread_cleanup_auto_required_advice()));
    }
    let staleness_cutoff = if args.auto {
        let spec = args
            .older_than
            .as_deref()
            .ok_or_else(|| anyhow!(thread_cleanup_older_than_required_advice()))?;
        Some(parse_duration(spec)?)
    } else {
        None
    };

    let manager = thread_manager(repo);
    let threads = manager.list()?;
    let now = Utc::now();

    // Cleanup running from inside a qualifying thread used to drop
    // its own checkout mid-command, leaving the user in a deleted
    // directory. Identify the active thread (best-effort: a missing
    // current_thread is non-fatal — we just don't skip anything) and
    // exclude it from the drop list, surfacing it as a skip instead.
    let active_thread_id: Option<String> = match current_thread(repo) {
        Ok(Some(t)) => Some(t.id),
        _ => None,
    };

    let mut merged_drops: Vec<(Thread, DroppedThread)> = Vec::new();
    let mut auto_drops: Vec<(Thread, DroppedThread)> = Vec::new();
    let mut skipped: Vec<SkippedThread> = Vec::new();

    for thread in threads {
        // Skip already-abandoned threads — `cmd_thread_drop` marks
        // dropped threads abandoned, so re-running cleanup should be
        // a no-op for them.
        if matches!(thread.state, ThreadState::Abandoned) {
            continue;
        }
        let age_seconds = (now - thread.updated_at).num_seconds().max(0);
        let bytes = directory_size(&thread.execution_path);
        let execution_path = thread
            .execution_path
            .to_str()
            .map(ToString::to_string)
            .filter(|s| !s.is_empty());

        let is_active = active_thread_id
            .as_deref()
            .is_some_and(|id| id == thread.id);

        if args.merged && matches!(thread.state, ThreadState::Merged) {
            if is_active {
                tracing::info!(
                    thread = %thread.thread,
                    "skipping cleanup of active thread (currently in use)"
                );
                skipped.push(SkippedThread {
                    thread: thread.thread.clone(),
                    id: thread.id.clone(),
                    reason: "active",
                    note:
                        "currently the active thread; would leave the user in a deleted directory"
                            .to_string(),
                });
                continue;
            }
            let dropped = DroppedThread {
                thread: thread.thread.clone(),
                id: thread.id.clone(),
                reason: "merged",
                age_seconds,
                bytes,
                execution_path: execution_path.clone(),
            };
            merged_drops.push((thread, dropped));
            continue;
        }

        if args.auto && thread.auto {
            if let Some(cutoff) = staleness_cutoff
                && age_seconds < cutoff.num_seconds()
            {
                continue;
            }
            if is_active {
                tracing::info!(
                    thread = %thread.thread,
                    "skipping cleanup of active thread (currently in use)"
                );
                skipped.push(SkippedThread {
                    thread: thread.thread.clone(),
                    id: thread.id.clone(),
                    reason: "active",
                    note:
                        "currently the active thread; would leave the user in a deleted directory"
                            .to_string(),
                });
                continue;
            }
            let dropped = DroppedThread {
                thread: thread.thread.clone(),
                id: thread.id.clone(),
                reason: "auto-stale",
                age_seconds,
                bytes,
                execution_path,
            };
            auto_drops.push((thread, dropped));
        }
    }

    let mut reclaimed_bytes: u64 = 0;
    if !args.dry_run {
        for (thread, dropped) in merged_drops.iter().chain(auto_drops.iter()) {
            apply_thread_drop(repo, &manager, thread)?;
            reclaimed_bytes = reclaimed_bytes.saturating_add(dropped.bytes);
        }
    }

    let merged_summary: Vec<DroppedThread> = merged_drops.iter().map(|(_, d)| d.clone()).collect();
    let auto_summary: Vec<DroppedThread> = auto_drops.iter().map(|(_, d)| d.clone()).collect();
    let total_dropped = merged_summary.len() + auto_summary.len();
    let would_reclaim: u64 = merged_summary
        .iter()
        .chain(auto_summary.iter())
        .map(|d| d.bytes)
        .sum();

    let action_word = if args.dry_run {
        "would drop"
    } else {
        "dropped"
    };
    let mut parts: Vec<String> = Vec::new();
    if args.merged {
        parts.push(format!(
            "{} {} merged thread(s)",
            action_word,
            merged_summary.len()
        ));
    }
    if args.auto {
        parts.push(format!(
            "{} {} stale auto-thread(s)",
            action_word,
            auto_summary.len()
        ));
    }
    let bytes_for_message = if args.dry_run {
        would_reclaim
    } else {
        reclaimed_bytes
    };
    let reclaim_word = if args.dry_run {
        "would reclaim"
    } else {
        "reclaimed"
    };
    let summary_message = format!(
        "{} ({} {})",
        parts.join(", "),
        reclaim_word,
        format_bytes(bytes_for_message)
    );

    // Dry runs leave disk untouched, so `reclaimed_bytes` must stay
    // `0` — automation watching it for actual reclaim would
    // otherwise see a phantom value. The estimate lives in
    // `would_reclaim_bytes` instead. In a real run the two match.
    let output = ThreadCleanupOutput {
        operator: OperatorCommandOutput {
            status: "completed".to_string(),
            action: "thread.cleanup".to_string(),
            message: summary_message.clone(),
            blockers: Vec::new(),
            warnings: Vec::new(),
            next_action: None,
            recommended_action: None,
        },
        dry_run: args.dry_run,
        merged: merged_summary.clone(),
        auto: auto_summary.clone(),
        reclaimed_bytes: if args.dry_run { 0 } else { reclaimed_bytes },
        would_reclaim_bytes: would_reclaim,
        skipped: skipped.clone(),
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("{}", summary_message);
        if total_dropped == 0 {
            println!("No threads matched the cleanup criteria.");
        } else {
            for entry in merged_summary.iter().chain(auto_summary.iter()) {
                println!(
                    "  - {} ({}) [{}]  {}  age {}s",
                    entry.thread,
                    entry.id,
                    entry.reason,
                    format_bytes(entry.bytes),
                    entry.age_seconds,
                );
            }
        }
        for entry in &skipped {
            println!(
                "  - {} ({}) [skipped: {}]  {}",
                entry.thread, entry.id, entry.reason, entry.note,
            );
        }
    }
    Ok(())
}

/// Apply a cleanup drop to a single thread.
///
/// Cleanup is stronger than an ordinary `thread drop`: once the user
/// has confirmed a merged/stale sweep, the thread should disappear
/// from everyday thread and push surfaces. We preserve an abandoned
/// manager record for audit/debugging, but remove the live thread ref
/// and manifest so `thread list` and `push --all-threads` no longer
/// treat the cleaned thread as active work.
///
/// Mounts are keyed by the thread *name* (`thread.thread`) — the same
/// value passed at mount time via `establish_virtualized_mount`. The
/// `ThreadRecord::id` may diverge from `ThreadRecord::thread` for
/// legacy/synced records, so unmounting by `thread.id` would miss the
/// live mount and let the subsequent `remove_path_recursively` fail
/// with EBUSY against the still-mounted path. See `mount_lifecycle`
/// for the keying convention.
fn apply_thread_drop(repo: &Repository, manager: &ThreadManager, thread: &Thread) -> Result<()> {
    if matches!(thread.mode, ThreadMode::Virtualized) {
        mount_lifecycle::unmount_thread_if_mounted(&thread.thread);
        if let Err(error) =
            crate::cli::commands::daemon_client::unmount_via_daemon(repo.root(), &thread.thread)
        {
            tracing::warn!(
                thread = %thread.thread,
                %error,
                "daemon unmount RPC failed during cleanup; continuing"
            );
        }
    }
    if thread.execution_path.exists() {
        remove_path_recursively(&thread.execution_path)?;
    }
    repo::thread_manifest::remove_thread_manifest_dir(repo.heddle_dir(), &thread.thread)?;
    let mut updated = thread.clone();
    updated.state = ThreadState::Abandoned;
    updated.updated_at = Utc::now();
    manager.save(&updated)?;
    let tn = ThreadName::new(&thread.thread);
    if repo.refs().get_thread(&tn)?.is_some() {
        repo.refs().delete_thread(&tn)?;
    }
    let registry = AgentRegistry::new(repo.heddle_dir());
    for entry in registry.list()? {
        if entry.thread == thread.thread || entry.thread_id.as_deref() == Some(&thread.id) {
            registry.delete(&entry.session_id)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;

    #[test]
    fn parse_duration_handles_supported_units() {
        assert_eq!(parse_duration("0").unwrap(), chrono::Duration::seconds(0));
        assert_eq!(parse_duration("90").unwrap(), chrono::Duration::seconds(90));
        assert_eq!(
            parse_duration("30s").unwrap(),
            chrono::Duration::seconds(30)
        );
        assert_eq!(
            parse_duration("15m").unwrap(),
            chrono::Duration::minutes(15)
        );
        assert_eq!(parse_duration("4h").unwrap(), chrono::Duration::hours(4));
        assert_eq!(parse_duration("7d").unwrap(), chrono::Duration::days(7));
        assert_eq!(parse_duration("2w").unwrap(), chrono::Duration::weeks(2));
    }

    #[test]
    fn parse_duration_rejects_unknown_units_and_negatives() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("3y").is_err());
        assert!(parse_duration("foo").is_err());
        assert!(parse_duration("-5").is_err());
    }

    /// chrono's `Duration::weeks` (and similar) panic on overflow.
    /// The CLI used to crash on inputs like `9223372036854775807w`.
    /// `parse_duration` must surface a graceful error instead.
    #[test]
    fn parse_duration_rejects_overflow() {
        let huge_weeks = format!("{}w", i64::MAX);
        let err = parse_duration(&huge_weeks).expect_err("must reject overflow");
        let msg = format!("{err}");
        assert!(
            msg.contains("overflow"),
            "error should mention overflow; got: {msg}"
        );

        // Same for days / hours / minutes — every multiplier-bearing
        // unit must be guarded.
        for suffix in ["d", "h", "m"] {
            let huge = format!("{}{}", i64::MAX, suffix);
            assert!(
                parse_duration(&huge).is_err(),
                "{suffix} overflow should error",
            );
        }
    }

    #[test]
    fn format_bytes_picks_a_reasonable_unit() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(1024 * 1024 * 5), "5.0 MB");
    }
}
