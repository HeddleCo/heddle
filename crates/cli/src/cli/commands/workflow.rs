// SPDX-License-Identifier: Apache-2.0
use anyhow::{Result, anyhow};
use repo::{Repository, Thread, ThreadIntegrationPolicy};
use serde::Serialize;

use super::{
    checkpoint::create_git_checkpoint,
    git_overlay_health::{RepositoryTrustState, build_repository_trust_state},
    merge::{build_thread_preview_report, merge_thread_into_current},
    operator_core::OperatorCommandOutput,
    operator_loop::primary_next_action,
    ready_cmd::worktree_dirty,
    snapshot::{SnapshotAgentOverrides, create_snapshot},
    thread::start_thread,
    thread_cmd::{current_thread, load_thread, refresh_thread, thread_manager},
};
use crate::{
    cli::{
        Cli, ThreadStartArgs, WorkspaceModeArg,
        cli_args::{DelegateArgs, ShipArgs, SyncArgs},
        should_output_json, worktree_status_options,
    },
    config::UserConfig,
};

#[derive(Serialize)]
struct SyncOutput {
    #[serde(flatten)]
    operator: OperatorCommandOutput,
    trust: RepositoryTrustState,
    thread: String,
    current_state: Option<String>,
    chosen_path: String,
}

#[derive(Serialize)]
struct ShipOutput {
    #[serde(flatten)]
    operator: OperatorCommandOutput,
    thread: String,
    captured: bool,
    checkpointed: bool,
    git_commit: Option<String>,
    synced: bool,
    integrated: bool,
    pushed: bool,
    merge_state: Option<String>,
    trust: RepositoryTrustState,
    chosen_path: String,
}

#[derive(Serialize)]
struct DelegatedThreadOutput {
    name: String,
    task: String,
    path: Option<String>,
    execution_path: Option<String>,
}

#[derive(Serialize)]
struct DelegateOutput {
    parent_thread: String,
    delegated: Vec<DelegatedThreadOutput>,
    message: String,
}

pub async fn cmd_sync(cli: &Cli, args: SyncArgs) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let mut thread = resolve_thread(&repo, args.thread.as_deref())?;

    let stale_report = build_thread_preview_report(&repo, &mut thread, true)?;
    let stale_blockers = non_staleness_blockers(&stale_report.blockers);
    let operation = repo.operation_status()?;
    let remote_tracking = repo.git_remote_tracking_status()?;
    let import_hint = repo.git_overlay_import_hint()?;
    let mut output = if thread.freshness == repo::ThreadFreshness::Current {
        let recommended_action = primary_next_action(
            operation.as_ref(),
            remote_tracking.as_ref(),
            import_hint.as_ref(),
            Some("heddle ship"),
        );
        let trust = build_repository_trust_state(&repo);
        SyncOutput {
            operator: OperatorCommandOutput {
                status: "current".to_string(),
                action: "sync".to_string(),
                message: format!("Thread '{}' is already current", thread.id),
                blockers: vec![],
                warnings: Vec::new(),
                next_action: Some(recommended_action.clone()),
                recommended_action: Some(recommended_action),
            },
            trust,
            thread: thread.id.clone(),
            current_state: thread.current_state.clone(),
            chosen_path: "no_op".to_string(),
        }
    } else if stale_report.conflict_count > 0 || !stale_blockers.is_empty() {
        let recommended_action = primary_next_action(
            operation.as_ref(),
            remote_tracking.as_ref(),
            import_hint.as_ref(),
            Some(&stale_report.recommended_action),
        );
        update_integration_policy(
            &repo,
            &thread.id,
            "blocked",
            stale_blockers
                .first()
                .cloned()
                .unwrap_or_else(|| "refresh requires manual resolution".to_string()),
        )?;
        let trust = build_repository_trust_state(&repo);
        SyncOutput {
            operator: OperatorCommandOutput {
                status: "blocked".to_string(),
                action: "sync".to_string(),
                message: format!("Thread '{}' needs manual refresh", thread.id),
                blockers: stale_report.blockers.clone(),
                warnings: Vec::new(),
                next_action: Some(recommended_action.clone()),
                recommended_action: Some(recommended_action),
            },
            trust,
            thread: thread.id.clone(),
            current_state: thread.current_state.clone(),
            chosen_path: "blocked".to_string(),
        }
    } else {
        let refreshed = refresh_thread(&repo, &thread.id, cli)?;
        update_integration_policy(&repo, &refreshed.id, "current", "thread refreshed cleanly")?;
        let recommended_action = primary_next_action(
            operation.as_ref(),
            remote_tracking.as_ref(),
            import_hint.as_ref(),
            Some("heddle ship"),
        );
        let trust = build_repository_trust_state(&repo);
        SyncOutput {
            operator: OperatorCommandOutput {
                status: "refreshed".to_string(),
                action: "sync".to_string(),
                message: format!("Refreshed thread '{}'", refreshed.id),
                blockers: vec![],
                warnings: Vec::new(),
                next_action: Some(recommended_action.clone()),
                recommended_action: Some(recommended_action),
            },
            trust,
            thread: refreshed.id.clone(),
            current_state: refreshed.current_state.clone(),
            chosen_path: "refresh".to_string(),
        }
    };
    block_operator_claim_if_trust_blocked(&mut output.operator, &output.trust);

    emit(cli, &output)
}

pub async fn cmd_ship(cli: &Cli, args: ShipArgs) -> Result<()> {
    // Open at CWD only to discover the active thread, then re-open at
    // its metadata-recorded worktree. This makes `heddle ship` work
    // from anywhere — operators don't need to `cd` into a lightweight
    // thread directory before shipping. The capture/merge below run
    // against `repo`, so they all see the same checkout. See
    // `Repository::active_worktree_path`.
    let cwd_repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let target_path = cwd_repo.active_worktree_path()?;
    let repo = if target_path == *cwd_repo.root() {
        cwd_repo
    } else {
        Repository::open(&target_path)?
    };
    let user_config = UserConfig::load_default().unwrap_or_default();
    let thread = resolve_thread(&repo, args.thread.as_deref())?;
    let thread_repo = Repository::open(&thread.execution_path)?;

    let mut captured = false;
    let status_options = worktree_status_options(Some(thread_repo.config()));
    if worktree_dirty(&thread_repo, &status_options)? {
        create_snapshot(
            &thread_repo,
            &user_config,
            args.message.clone(),
            None,
            SnapshotAgentOverrides {
                provider: None,
                model: None,
                session: None,
                segment: None,
                policy: None,
                no_policy: false,
                no_agent: false,
            },
        )?;
        captured = true;
    }

    let mut synced = false;
    let mut refreshed_thread = resolve_thread(&repo, Some(&thread.id))?;
    if refreshed_thread.freshness == repo::ThreadFreshness::Stale {
        let preview = build_thread_preview_report(&repo, &mut refreshed_thread, true)?;
        let stale_blockers = non_staleness_blockers(&preview.blockers);
        if preview.conflict_count > 0 || !stale_blockers.is_empty() {
            update_integration_policy(
                &repo,
                &refreshed_thread.id,
                "blocked",
                stale_blockers
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "refresh requires manual resolution".to_string()),
            )?;
            return emit(
                cli,
                &ShipOutput {
                    operator: OperatorCommandOutput {
                        status: "blocked".to_string(),
                        action: "ship".to_string(),
                        message: format!(
                            "Thread '{}' must be refreshed manually",
                            refreshed_thread.id
                        ),
                        blockers: preview.blockers.clone(),
                        warnings: Vec::new(),
                        next_action: Some(preview.recommended_action.clone()),
                        recommended_action: Some(preview.recommended_action),
                    },
                    thread: refreshed_thread.id.clone(),
                    captured,
                    checkpointed: false,
                    git_commit: None,
                    synced: false,
                    integrated: false,
                    pushed: false,
                    merge_state: None,
                    trust: build_repository_trust_state(&repo),
                    chosen_path: "blocked".to_string(),
                },
            );
        }

        refreshed_thread = refresh_thread(&repo, &refreshed_thread.id, cli)?;
        synced = true;
    }

    let mut merge_thread = resolve_thread(&repo, Some(&refreshed_thread.id))?;
    let preview = build_thread_preview_report(&repo, &mut merge_thread, true)?;
    let integration_blockers = integration_blockers(&repo, &merge_thread, &preview);
    let manual_resolution_current = manual_resolution_current(&repo, &merge_thread);
    if manual_resolution_current {
        let merge_state = adopt_manual_resolution(&repo, &merge_thread.id)?;
        let mut checkpointed = false;
        let mut git_commit = None;
        update_integration_policy(
            &repo,
            &merge_thread.id,
            "auto_integrated",
            "accepted manually resolved integration state",
        )?;
        if repo.capability() == repo::RepositoryCapability::GitOverlay {
            let record = create_git_checkpoint(
                &repo,
                args.message
                    .as_deref()
                    .or(Some(&format!("Ship {}", merge_thread.id))),
                worktree_status_options(Some(repo.config())),
            )?;
            checkpointed = true;
            git_commit = Some(record.git_commit);
        }
        clear_manual_resolution_state(&repo, &merge_thread.id)?;
        let trust = build_repository_trust_state(&repo);
        let mut operator = OperatorCommandOutput {
            status: "shipped".to_string(),
            action: "ship".to_string(),
            message: format!(
                "Shipped thread '{}' from a manually resolved integration state",
                merge_thread.id
            ),
            blockers: Vec::new(),
            warnings: Vec::new(),
            next_action: None,
            recommended_action: None,
        };
        block_operator_claim_if_trust_blocked(&mut operator, &trust);
        return emit(
            cli,
            &ShipOutput {
                operator,
                thread: merge_thread.id.clone(),
                captured,
                checkpointed,
                git_commit,
                synced,
                integrated: true,
                pushed: false,
                merge_state: Some(merge_state),
                trust,
                chosen_path: if checkpointed {
                    "capture_sync_manual_resolution_checkpoint".to_string()
                } else {
                    "capture_sync_manual_resolution".to_string()
                },
            },
        );
    }
    if preview.conflict_count > 0 || !integration_blockers.is_empty() {
        let reason = integration_blockers
            .first()
            .cloned()
            .unwrap_or_else(|| "integration requires manual review".to_string());
        update_integration_policy(&repo, &merge_thread.id, "blocked", &reason)?;
        return emit(
            cli,
            &ShipOutput {
                operator: OperatorCommandOutput {
                    status: "blocked".to_string(),
                    action: "ship".to_string(),
                    message: format!("Thread '{}' is not eligible for auto-ship", merge_thread.id),
                    blockers: integration_blockers.clone(),
                    warnings: Vec::new(),
                    next_action: Some(preview.recommended_action.clone()),
                    recommended_action: Some(preview.recommended_action),
                },
                thread: merge_thread.id.clone(),
                captured,
                checkpointed: false,
                git_commit: None,
                synced,
                integrated: false,
                pushed: false,
                merge_state: None,
                trust: build_repository_trust_state(&repo),
                chosen_path: "blocked".to_string(),
            },
        );
    }

    let merge_output = merge_thread_into_current(
        &repo,
        &merge_thread.id,
        None,
        false,
        false,
        false,
        false,
        false,
    )?;
    let integrated = merge_output.conflicts.is_empty() && merge_output.merge_state.is_some();
    let mut checkpointed = false;
    let mut git_commit = None;
    update_integration_policy(
        &repo,
        &merge_thread.id,
        if integrated {
            "auto_integrated"
        } else {
            "blocked"
        },
        if integrated {
            "clean integration path"
        } else {
            "merge produced conflicts"
        },
    )?;

    if integrated && repo.capability() == repo::RepositoryCapability::GitOverlay {
        let record = create_git_checkpoint(
            &repo,
            args.message
                .as_deref()
                .or(Some(&format!("Ship {}", merge_thread.id))),
            worktree_status_options(Some(repo.config())),
        )?;
        checkpointed = true;
        git_commit = Some(record.git_commit);
    }

    let should_push = args.push && !args.no_push;
    let mut pushed = false;
    if integrated && should_push {
        super::remote::cmd_push(
            cli,
            args.remote.clone(),
            None,
            merge_output.merge_state.clone(),
            false,
        )
        .await?;
        pushed = true;
    }

    if integrated {
        clear_manual_resolution_state(&repo, &merge_thread.id)?;
    }

    let trust = build_repository_trust_state(&repo);
    let mut operator = OperatorCommandOutput {
        status: if integrated { "shipped" } else { "blocked" }.to_string(),
        action: "ship".to_string(),
        message: if integrated {
            format!("Shipped thread '{}'", merge_thread.id)
        } else {
            format!("Thread '{}' could not be shipped cleanly", merge_thread.id)
        },
        blockers: merge_output.operator.blockers.clone(),
        warnings: Vec::new(),
        next_action: if integrated {
            None
        } else {
            merge_output.operator.recommended_action.clone()
        },
        recommended_action: if integrated {
            None
        } else {
            merge_output.operator.recommended_action.clone()
        },
    };
    block_operator_claim_if_trust_blocked(&mut operator, &trust);

    emit(
        cli,
        &ShipOutput {
            operator,
            thread: merge_thread.id.clone(),
            captured,
            checkpointed,
            git_commit,
            synced,
            integrated,
            pushed,
            merge_state: merge_output.merge_state.clone(),
            trust,
            chosen_path: if integrated {
                if pushed {
                    "capture_sync_merge_checkpoint_push"
                } else if checkpointed {
                    "capture_sync_merge_checkpoint"
                } else {
                    "capture_sync_merge"
                }
                .to_string()
            } else {
                "blocked".to_string()
            },
        },
    )
}

fn block_operator_claim_if_trust_blocked(
    operator: &mut OperatorCommandOutput,
    trust: &RepositoryTrustState,
) {
    if trust.trusted || operator.status == "blocked" || operator.status == "failed" {
        return;
    }

    operator.status = "blocked".to_string();
    operator.message = format!(
        "{} reached its local state checks, but repository trust is blocked: {}",
        operator.action, trust.summary
    );
    operator.blockers = trust
        .checks
        .iter()
        .filter(|check| !check.clean)
        .map(|check| format!("{}: {}", check.name, check.summary))
        .collect();
    let recommended_action = if trust.recommended_action.is_empty() {
        "heddle trust".to_string()
    } else {
        trust.recommended_action.clone()
    };
    operator.next_action = Some(recommended_action.clone());
    operator.recommended_action = Some(recommended_action);
}

pub fn cmd_delegate(cli: &Cli, args: DelegateArgs) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    warn_if_path_prefix_inside_repo(&repo, args.path_prefix.as_deref());
    let parent = resolve_parent_thread(&repo, args.parent.as_deref())?;

    // Warm the canonical loose-uncompressed store for the parent
    // state once, before we materialize it into N child worktrees.
    // The first child would otherwise pay
    // `decompress + atomic write` per blob (lazy promotion inside
    // `materialize_blob`), and only worktrees 2..N would hardlink.
    // A single warm pass amortizes promotion cost across all N
    // children in the common N-agents-on-the-same-parent case.
    //
    // Failures are non-fatal: the lazy path inside
    // `materialize_blob` will still promote on demand, and an empty
    // or partially-warm store just means the first materialize pays
    // promotion cost for any blobs we missed.
    if args.tasks.len() > 1 {
        let parent_state_spec = parent
            .current_state
            .clone()
            .unwrap_or_else(|| parent.base_state.clone());
        match repo
            .resolve_state(&parent_state_spec)
            .ok()
            .and_then(|opt| opt)
        {
            Some(parent_state_id) => match repo.warm_canonical_store_for_state(&parent_state_id) {
                Ok(stats) => {
                    tracing::debug!(
                        promoted = stats.promoted,
                        already_loose = stats.already_loose,
                        errors = stats.errors,
                        "Warmed canonical store before delegate fan-out"
                    );
                }
                Err(err) => {
                    tracing::debug!(
                        ?err,
                        "Warm canonical store failed; falling back to lazy promotion in materialize"
                    );
                }
            },
            None => {
                tracing::debug!(
                    parent_state = %parent_state_spec,
                    "Could not resolve parent state for warm pass; falling back to lazy promotion"
                );
            }
        }
    }

    // Pre-flight: when two specs collapse to the same slug (e.g.
    // racing three agents on a "modulo" task with all three entries
    // labelled "modulo:..."), `start_thread` would refuse the duplicate
    // thread name halfway through and leave a partial workspace
    // behind. Disambiguate by suffixing the slug with the provider
    // when collisions exist. Pure heads-up logic — no behavior change
    // for the single-agent-per-task case.
    let mut slug_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for spec in &args.tasks {
        *slug_counts.entry(slugify(&spec.task)).or_insert(0) += 1;
    }

    let delegated = args
        .tasks
        .iter()
        .map(|spec| {
            let base_slug = slugify(&spec.task);
            let slug = if slug_counts.get(&base_slug).copied().unwrap_or(0) > 1 {
                match spec.provider.as_deref() {
                    Some(provider) => format!("{base_slug}-{}", slugify(provider)),
                    None => base_slug.clone(),
                }
            } else {
                base_slug
            };
            let name = format!("{}/{}", parent.id, slug);
            let path = args.path_prefix.as_ref().map(|prefix| prefix.join(&slug));

            // Per-spec agent override wins; fall back to the
            // command-wide default (`--agent-provider`/`--agent-model`).
            let agent_provider = spec
                .provider
                .clone()
                .or_else(|| args.agent_provider.clone());
            let agent_model = spec.model.clone().or_else(|| args.agent_model.clone());

            let output = start_thread(
                &repo,
                ThreadStartArgs {
                    name: name.clone(),
                    from: Some(
                        parent
                            .current_state
                            .clone()
                            .unwrap_or(parent.base_state.clone()),
                    ),
                    path,
                    workspace: args.workspace.unwrap_or(WorkspaceModeArg::Auto),
                    agent_provider,
                    agent_model,
                    task: Some(spec.task.clone()),
                    parent_thread: Some(parent.id.clone()),
                    automated: true,
                    print_cd_path: false,
                    // Delegated children inherit the in-process mount path
                    // explicitly: spawning a `heddled` daemon as a side
                    // effect of `heddle delegate` would surprise the
                    // caller (delegate is mostly used with materialized /
                    // lightweight workspaces anyway). If a future caller
                    // passes `--workspace virtualized` through delegate
                    // and wants daemon ownership, they can spawn the
                    // daemon explicitly first.
                    daemon: false,
                    no_daemon: true,
                    // Delegated children inherit the parent's
                    // implicit per-checkout target/. If a delegate
                    // user wants the shared-target arrangement they
                    // can opt in by re-running `heddle start
                    // --shared-target` against the spawned thread —
                    // delegate is a thin orchestration verb and
                    // shouldn't make filesystem-layout decisions for
                    // the user.
                    shared_target: false,
                },
            )?;
            Ok(DelegatedThreadOutput {
                name,
                task: spec.task.clone(),
                path: output.path,
                execution_path: output.execution_path,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    emit(
        cli,
        &DelegateOutput {
            parent_thread: parent.id,
            delegated,
            message: "Delegated child threads created".to_string(),
        },
    )
}

/// Print a one-line warning when the operator passes
/// `--path-prefix <path>` and `<path>` (after resolving against CWD)
/// is a strict descendant of the repo root. The new
/// nested-thread-worktree exclusion in `repo` makes this layout safe,
/// but the conventional shape is a sibling directory; flagging the
/// unconventional choice keeps the demo geometry honest.
fn warn_if_path_prefix_inside_repo(repo: &Repository, path_prefix: Option<&std::path::Path>) {
    let Some(prefix) = path_prefix else {
        return;
    };
    let resolved = if prefix.is_absolute() {
        prefix.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(prefix),
            Err(_) => return,
        }
    };
    let canonical_prefix = resolved.canonicalize().unwrap_or(resolved);
    let canonical_root = repo
        .root()
        .canonicalize()
        .unwrap_or_else(|_| repo.root().to_path_buf());
    if canonical_prefix == canonical_root {
        return;
    }
    if !canonical_prefix.starts_with(&canonical_root) {
        return;
    }
    eprintln!(
        "warn: agent worktree at {} is nested inside repo root {}; \
         the parent thread's scans will exclude it, but a sibling path is more conventional.",
        canonical_prefix.display(),
        canonical_root.display(),
    );
}

fn resolve_thread(repo: &Repository, thread: Option<&str>) -> Result<Thread> {
    match thread {
        Some(thread) => load_thread(repo, thread),
        None => current_thread(repo)?
            .ok_or_else(|| anyhow!("No current thread; pass --thread or run inside a thread")),
    }
}

fn resolve_parent_thread(repo: &Repository, thread: Option<&str>) -> Result<Thread> {
    resolve_thread(repo, thread).or_else(|_| {
        let head = repo.head_ref()?;
        match head {
            refs::Head::Attached { thread } => load_thread(repo, &thread),
            refs::Head::Detached { .. } => Err(anyhow!("No attached parent thread; pass --parent")),
        }
    })
}

fn update_integration_policy(
    repo: &Repository,
    thread_id: &str,
    status: &str,
    reason: impl Into<String>,
) -> Result<()> {
    let manager = thread_manager(repo);
    let mut thread = manager
        .load(thread_id)?
        .ok_or_else(|| anyhow!("Thread '{}' not found", thread_id))?;
    thread.integration_policy_result = ThreadIntegrationPolicy {
        status: Some(status.to_string()),
        reason: Some(reason.into()),
        manual_resolution_state: thread.integration_policy_result.manual_resolution_state,
    };
    manager.save(&thread)?;
    Ok(())
}

fn clear_manual_resolution_state(repo: &Repository, thread_id: &str) -> Result<()> {
    let manager = thread_manager(repo);
    let mut thread = manager
        .load(thread_id)?
        .ok_or_else(|| anyhow!("Thread '{}' not found", thread_id))?;
    thread.integration_policy_result.manual_resolution_state = None;
    Ok(manager.save(&thread)?)
}

fn adopt_manual_resolution(repo: &Repository, thread_id: &str) -> Result<String> {
    let manager = thread_manager(repo);
    let mut thread = manager
        .load(thread_id)?
        .ok_or_else(|| anyhow!("Thread '{}' not found", thread_id))?;
    let target = repo.refs().get_thread(&thread.thread)?.ok_or_else(|| {
        anyhow!(
            "Thread '{}' has no current state to integrate",
            thread.thread
        )
    })?;
    super::ff_record::record_ff_advance(repo, &thread.thread, &target)?;
    thread.state = repo::ThreadState::Merged;
    thread.merged_state = Some(target.short());
    thread.current_state = Some(target.short());
    thread.updated_at = chrono::Utc::now();
    thread.freshness = repo::ThreadFreshness::Current;
    manager.save(&thread)?;
    Ok(target.short())
}

fn integration_blockers(
    repo: &Repository,
    thread: &Thread,
    preview: &super::merge::ThreadPreviewReport,
) -> Vec<String> {
    let manual_resolution_current = manual_resolution_current(repo, thread);
    let mut blockers = if manual_resolution_current {
        Vec::new()
    } else {
        non_staleness_blockers(&preview.blockers)
    };
    let agent_authored = thread_is_agent_authored(repo, thread);
    if agent_authored {
        if let Some(confidence) = thread.confidence_summary.value
            && confidence < 0.75
        {
            blockers.push(format!(
                "confidence {:.2} is below the auto-ship threshold of 0.75",
                confidence
            ));
        }
        if thread.confidence_summary.value.is_none() {
            blockers.push("confidence summary is missing for the current thread state".to_string());
        }
    }
    if matches!(thread.verification_summary.tests_passed, Some(false)) {
        blockers.push("verification summary reports failing tests".to_string());
    }
    blockers
}

fn manual_resolution_current(repo: &Repository, thread: &Thread) -> bool {
    let thread_tip = repo
        .refs()
        .get_thread(&thread.thread)
        .ok()
        .flatten()
        .map(|id| id.short());
    thread
        .integration_policy_result
        .manual_resolution_state
        .as_deref()
        .zip(thread_tip.as_deref())
        .is_some_and(|(resolved, current)| resolved == current)
        && thread.freshness == repo::ThreadFreshness::Current
}

fn thread_is_agent_authored(repo: &Repository, thread: &Thread) -> bool {
    let current_state = thread
        .current_state
        .as_deref()
        .and_then(|state| repo.resolve_state(state).ok().flatten())
        .or_else(|| repo.refs().get_thread(&thread.thread).ok().flatten());
    current_state
        .and_then(|id| repo.store().get_state(&id).ok().flatten())
        .map(|state| state.attribution.agent.is_some())
        .unwrap_or(false)
}

fn non_staleness_blockers(blockers: &[String]) -> Vec<String> {
    blockers
        .iter()
        .filter(|blocker| !blocker.contains(" is stale against "))
        .cloned()
        .collect()
}

fn slugify(input: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in input.chars().flat_map(|c| c.to_lowercase()) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn emit<T: Serialize>(cli: &Cli, output: &T) -> Result<()> {
    if should_output_json(cli, None) {
        println!("{}", serde_json::to_string(output)?);
    } else {
        println!("{}", serde_json::to_string_pretty(output)?);
    }
    Ok(())
}
