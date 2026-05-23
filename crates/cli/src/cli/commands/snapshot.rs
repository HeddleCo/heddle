// SPDX-License-Identifier: Apache-2.0
//! Snapshot command.

use std::time::Instant;

use anyhow::{Result, anyhow};
use objects::object::{Agent, Attribution, ChangeId, Principal, Tree};
use repo::{
    Hook, HookContext, HookManager, Repository, SessionManager, SnapshotProfile, format_confidence,
    refresh_active_thread_metadata,
};
// Re-export the helper derivations so existing CLI call sites
// (`thread.rs`, `harness/mod.rs`) keep `super::snapshot::summarize_*`
// imports working without churn. The implementations live in
// `repo::snapshot_metadata` so the mount and CLI paths share the same
// logic.
pub(crate) use repo::{summarize_confidence, summarize_verification};
use serde::Serialize;
use tracing::{debug, info};

use super::{advice::RecoveryAdvice, thread::find_active_thread_entry, thread_cmd::current_thread};
use crate::{
    cli::{Cli, should_output_json, style},
    config::UserConfig,
};

#[derive(Serialize)]
pub(crate) struct SnapshotOutput {
    pub change_id: String,
    pub content_hash: String,
    pub intent: Option<String>,
    pub confidence: Option<f32>,
    pub promotion_suggested: bool,
    pub heavy_impact_paths: Vec<String>,
    pub message: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SnapshotCommandProfile {
    pub tree_walk_ms: u128,
    pub blob_prep_ms: u128,
    pub blob_write_ms: u128,
    pub tree_write_ms: u128,
    pub state_ref_oplog_ms: u128,
    pub thread_metadata_ms: u128,
}

#[derive(Clone, Debug)]
pub struct SnapshotAgentOverrides {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub session: Option<String>,
    pub segment: Option<String>,
    pub policy: Option<String>,
    pub no_policy: bool,
    pub no_agent: bool,
}

/// Stable exit code emitted when capture aborts because the filesystem
/// is out of space. Mirrors the POSIX ENOSPC value (28) so shell users
/// and supervisors that already classify "disk full" by OS code see a
/// matching signal from heddle.
pub const CAPTURE_EXIT_DISK_FULL: i32 = 28;

pub async fn cmd_snapshot(
    cli: &Cli,
    intent: Option<String>,
    confidence: Option<f32>,
    force: bool,
    agent: SnapshotAgentOverrides,
) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let user_config = UserConfig::load_default().unwrap_or_default();

    preflight_large_capture(&repo, force)?;
    let output = match create_snapshot(&repo, &user_config, intent, confidence, agent) {
        Ok(output) => output,
        Err(err) => {
            // ENOSPC is the only mid-capture failure where the user's
            // working tree is guaranteed safe (we never touched it) and
            // the recovery is mechanical (free disk, re-run). Surface
            // that contract explicitly: a clean stderr line, a stable
            // exit code, and no anyhow stack-noise. Every other error
            // bubbles through `?` unchanged so the existing diagnostics
            // path keeps working.
            if is_disk_full_anyhow(&err) {
                eprintln!("error: {err:#}");
                eprintln!(
                    "hint: free disk space and re-run `heddle capture`. Your working tree changes are intact."
                );
                std::process::exit(CAPTURE_EXIT_DISK_FULL);
            }
            return Err(err);
        }
    };

    let as_json = should_output_json(cli, Some(repo.config()));
    if as_json {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        // The bare `{message}` was `"Created state <id> (<hash>)"` —
        // we restyle the parts here rather than inside the message
        // builder so JSON consumers (which read `output.message`)
        // continue to receive a clean ANSI-free string.
        println!(
            "Captured state {} ({})",
            style::change_id(&output.change_id),
            style::dim(&output.content_hash),
        );
        // Surface the recorded confidence so users see the value that
        // actually landed on the state — including when it's absent.
        // An unset confidence renders as `—`; never as `0.00`. The
        // shared helper lives in `repo::snapshot_metadata` so
        // `show.rs` and `log.rs` agree on the same sentinel.
        let confidence_text = format_confidence(output.confidence);
        println!(
            "Confidence: {}",
            style::confidence(output.confidence, &confidence_text)
        );
        if output.promotion_suggested && !output.heavy_impact_paths.is_empty() {
            println!(
                "{}: {}",
                style::warn("Heavy-impact change"),
                crate::cli::render::preview_list(
                    &output.heavy_impact_paths,
                    output.heavy_impact_paths.len(),
                )
            );
        }
    }

    // Discoverability tip after a successful capture: nudge
    // toward `heddle checkpoint` for cheaper mid-stream saves. Only
    // fires once per session per repo, never on `--json`.
    crate::cli::tips::maybe_emit(
        repo.root(),
        Some(repo.config()),
        crate::cli::tips::Tip::CheckpointAfterCapture,
        as_json,
        cli.quiet,
    );

    Ok(())
}

/// Resolve the current thread name for hook payloads. Returns `""`
/// when HEAD is detached (no thread context); the hook protocol uses
/// the same empty-string sentinel.
fn current_thread_name(repo: &Repository) -> String {
    use refs::Head;
    match repo.head_ref() {
        Ok(Head::Attached { thread }) => thread,
        _ => String::new(),
    }
}

pub(crate) fn preflight_large_capture_for_compat_commit(
    repo: &Repository,
    force: bool,
) -> Result<()> {
    preflight_large_capture(repo, force)
}

fn preflight_large_capture(repo: &Repository, force: bool) -> Result<()> {
    if force {
        return Ok(());
    }

    let Ok(Some(status)) = repo.git_overlay_worktree_status() else {
        return Ok(());
    };

    let total = status.change_count();
    let delete_count = status.deleted.len();
    let add_count = status.added.len();
    let large_capture = total > 100 || delete_count > 25 || add_count > 100;
    if !large_capture {
        return Ok(());
    }

    let sample = status
        .deleted
        .iter()
        .chain(status.added.iter())
        .chain(status.modified.iter())
        .take(5)
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    anyhow::bail!(
        "Large capture safety check: this would capture {total} changed paths ({delete_count} deletions, {add_count} additions).\n\
         Sample: {sample}\n\
         If this is intentional, rerun with `heddle capture --force -m \"...\"`."
    );
}

pub(crate) fn create_snapshot(
    repo: &Repository,
    user_config: &UserConfig,
    intent: Option<String>,
    confidence: Option<f32>,
    agent: SnapshotAgentOverrides,
) -> Result<SnapshotOutput> {
    create_snapshot_profiled(repo, user_config, intent, confidence, agent).map(|(output, _)| output)
}

pub(crate) fn ensure_current_state(
    repo: &Repository,
    user_config: &UserConfig,
    intent: Option<String>,
) -> Result<ChangeId> {
    if let Some(state) = repo.current_state()? {
        return Ok(state.change_id);
    }

    create_snapshot(
        repo,
        user_config,
        intent.or_else(|| Some(default_bootstrap_intent(repo))),
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

    repo.head()?
        .ok_or_else(|| anyhow::anyhow!("Failed to establish initial current state"))
}

pub(crate) fn create_snapshot_profiled(
    repo: &Repository,
    user_config: &UserConfig,
    intent: Option<String>,
    confidence: Option<f32>,
    agent: SnapshotAgentOverrides,
) -> Result<(SnapshotOutput, SnapshotCommandProfile)> {
    info!("Creating snapshot");

    let hook_manager = HookManager::new(repo);
    let hook_ctx = HookContext::new(repo);

    hook_manager.run(Hook::PreSnapshot, &hook_ctx)?;

    // JSON-protocol `pre_capture` invocation. Same hook
    // file as the legacy env-var path; the new protocol opts in via
    // `HEDDLE_HOOK_PROTOCOL=json` and gives the hook a chance to
    // veto. A non-empty `abort` aborts the snapshot.
    let pre_capture_payload = serde_json::json!({
        "thread": current_thread_name(repo),
        "intent": intent.clone().unwrap_or_default(),
    });
    let pre_capture_response = hook_manager.run_with_payload(
        Hook::PreSnapshot,
        &hook_ctx,
        &pre_capture_payload,
        std::time::Duration::from_secs(5),
    )?;
    if let Some(resp) = pre_capture_response
        && !resp.abort.is_empty()
    {
        return Err(anyhow!(RecoveryAdvice::hook_veto(
            "pre_capture",
            "capture",
            resp.abort
        )));
    }

    let attribution = build_attribution(repo, user_config, &agent)?;

    if let Some(ref agent) = attribution.agent {
        debug!(provider = %agent.provider, model = %agent.model, "Agent attribution");
    }

    let mut execution =
        repo.snapshot_with_attribution_profiled(intent.clone(), confidence, attribution)?;
    let thread_metadata_start = Instant::now();
    let (promotion_suggested, heavy_impact_paths) =
        update_active_thread_metadata(repo, &execution.state, &execution.tree)?;
    let thread_metadata_ms = thread_metadata_start.elapsed().as_millis();

    let output = SnapshotOutput {
        change_id: execution.state.change_id.short(),
        content_hash: execution.state.hash().short(),
        intent: execution.state.intent.clone(),
        confidence: execution.state.confidence,
        promotion_suggested,
        heavy_impact_paths: heavy_impact_paths.clone(),
        message: format!(
            "Captured state {} ({})",
            execution.state.change_id.short(),
            execution.state.hash().short()
        ),
    };

    hook_manager.run(Hook::PostSnapshot, &hook_ctx)?;

    // `post_capture` JSON-protocol fire. Best-effort: a
    // post-capture hook can't veto the snapshot (already persisted).
    // A timeout/error is tracing-warned and swallowed.
    let post_capture_payload = serde_json::json!({
        "state_id": execution.state.change_id.to_string_full(),
    });
    if let Err(err) = hook_manager.run_with_payload(
        Hook::PostSnapshot,
        &hook_ctx,
        &post_capture_payload,
        std::time::Duration::from_secs(5),
    ) {
        tracing::warn!(error = %err, "post_capture hook error swallowed");
    }

    Ok((
        output,
        snapshot_command_profile(execution.profile, thread_metadata_ms),
    ))
}

fn update_active_thread_metadata(
    repo: &Repository,
    state: &objects::object::State,
    tree: &Tree,
) -> Result<(bool, Vec<String>)> {
    let refresh = refresh_active_thread_metadata(repo, state, tree)?;
    Ok((refresh.promotion_suggested, refresh.heavy_impact_paths))
}

fn default_bootstrap_intent(repo: &Repository) -> String {
    match repo.head_ref() {
        Ok(refs::Head::Attached { thread }) => format!("Bootstrap git-overlay on {}", thread),
        _ => "Bootstrap git-overlay state".to_string(),
    }
}

fn snapshot_command_profile(
    repo_profile: SnapshotProfile,
    thread_metadata_ms: u128,
) -> SnapshotCommandProfile {
    SnapshotCommandProfile {
        tree_walk_ms: repo_profile.tree_walk_ms,
        blob_prep_ms: repo_profile.blob_prep_ms,
        blob_write_ms: repo_profile.blob_write_ms,
        tree_write_ms: repo_profile.tree_write_ms,
        state_ref_oplog_ms: repo_profile.state_ref_oplog_ms,
        thread_metadata_ms,
    }
}

fn build_attribution(
    repo: &Repository,
    user_config: &UserConfig,
    agent: &SnapshotAgentOverrides,
) -> Result<Attribution> {
    let principal = resolve_principal(repo, user_config)?;

    if agent.no_agent {
        return Ok(Attribution::human(principal));
    }

    let current_session = SessionManager::new(repo.root()).get_current_session()?;

    // Pull the thread's declared actor — set when the user ran
    // `heddle start --agent-provider X --agent-model Y` to dedicate this
    // thread to a specific agent. The `start` command writes an
    // `AgentEntry` into the `AgentRegistry`; `heddle status` already
    // surfaces it via `build_thread_view`. We look it up here so
    // `heddle capture` propagates it onto the resulting state's
    // `attribution.agent` — otherwise every captured state on an agent
    // thread would show `Principal: Unknown`, which broke the
    // provenance demo and the `heddle blame --context` story.
    //
    // Precedence: this slots in *after* explicit CLI overrides but
    // *before* the ambient HEDDLE_AGENT_* env (those reflect whoever
    // happens to be running heddle right now; the thread's actor is
    // the user's stated intent for *this* thread, which is more
    // specific). Falls back to the rest of the cascade unchanged.
    let thread_actor = current_thread(repo)
        .ok()
        .flatten()
        .and_then(|t| find_active_thread_entry(repo, &t.id).ok().flatten());
    // Harness probing writes the literal "unknown" placeholder into
    // `AgentEntry.model` and `SessionSegment.model` when it can't
    // identify the model from argv/env (see `harness::open_session`
    // and `claude_hook::handle_user_prompt_segment_rotate`). If we
    // let that placeholder participate in the precedence chain, an
    // ambient `HEDDLE_AGENT_MODEL=claude-opus-4-7` set by the user
    // never wins — captures keep surfacing as `anthropic/unknown`.
    // Strip the placeholder at every non-env source so explicit env
    // vars and config can fill in real data.
    let thread_provider = thread_actor
        .as_ref()
        .and_then(|e| e.provider.clone())
        .and_then(clean_attribution_value);
    let thread_model = thread_actor
        .as_ref()
        .and_then(|e| e.model.clone())
        .and_then(clean_attribution_value);

    let provider = agent
        .provider
        .clone()
        .or(thread_provider)
        .or_else(|| std::env::var("HEDDLE_AGENT_PROVIDER").ok())
        .or_else(|| {
            current_session
                .as_ref()
                .and_then(|session| session.current_segment())
                .map(|segment| segment.provider.clone())
                .and_then(clean_attribution_value)
        })
        .or_else(|| user_config.agent.provider.clone())
        .or_else(|| repo.config().agent.provider.clone());
    let model = agent
        .model
        .clone()
        .or(thread_model)
        .or_else(|| std::env::var("HEDDLE_AGENT_MODEL").ok())
        .or_else(|| {
            current_session
                .as_ref()
                .and_then(|session| session.current_segment())
                .map(|segment| segment.model.clone())
                .and_then(clean_attribution_value)
        })
        .or_else(|| user_config.agent.model.clone())
        .or_else(|| repo.config().agent.model.clone());
    let session_id = agent
        .session
        .clone()
        .or_else(|| std::env::var("HEDDLE_SESSION_ID").ok())
        .or_else(|| current_session.as_ref().map(|session| session.id.clone()));
    let segment_id = agent
        .segment
        .clone()
        .or_else(|| std::env::var("HEDDLE_SESSION_SEGMENT").ok())
        .or_else(|| {
            current_session
                .as_ref()
                .and_then(|session| session.current_segment_id.clone())
        });
    let policy = if agent.no_policy {
        None
    } else {
        agent
            .policy
            .clone()
            .or_else(|| std::env::var("HEDDLE_AGENT_POLICY").ok())
            .or_else(|| user_config.agent.default_policy.clone())
            .or_else(|| repo.config().policies.default_policy.clone())
    };

    match (provider, model) {
        (Some(p), Some(m)) => {
            let mut agent = Agent::new(p, m);
            if let (Some(sid), Some(segid)) = (session_id, segment_id) {
                agent = agent.with_session(sid, segid);
            }
            if let Some(pol) = policy {
                agent = agent.with_policy(pol);
            }
            Ok(Attribution::with_agent(principal, agent))
        }
        _ => Ok(Attribution::human(principal)),
    }
}

/// Treat the `"unknown"` harness placeholder and empty/whitespace
/// strings as absent so they don't beat real env-var or config
/// values in the attribution precedence chain.
fn clean_attribution_value(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("unknown") {
        None
    } else {
        Some(value)
    }
}

/// Resolve the human + agent attribution for a non-capture command (context,
/// fork, collapse, etc.). Mirrors the principal precedence chain that snapshot
/// uses (env > repo > user > Unknown) and attaches the ambient agent from the
/// same env/repo lookup `Repository::resolve_agent` performs.
///
/// Differs from the snapshot path in two ways — both intentional: it does not
/// honor explicit `--agent-*` flag overrides (other commands don't expose
/// those), and it does not consult the active `heddle session` chain. Use the
/// snapshot path's full `resolve_*` for capture flows.
pub(crate) fn resolve_attribution(
    repo: &Repository,
    user_config: &UserConfig,
) -> Result<Attribution> {
    let principal = resolve_principal(repo, user_config)?;
    let agent_provider = std::env::var("HEDDLE_AGENT_PROVIDER")
        .ok()
        .and_then(clean_attribution_value)
        .or_else(|| {
            user_config
                .agent
                .provider
                .clone()
                .and_then(clean_attribution_value)
        })
        .or_else(|| {
            repo.config()
                .agent
                .provider
                .clone()
                .and_then(clean_attribution_value)
        });
    let agent_model = std::env::var("HEDDLE_AGENT_MODEL")
        .ok()
        .and_then(clean_attribution_value)
        .or_else(|| {
            user_config
                .agent
                .model
                .clone()
                .and_then(clean_attribution_value)
        })
        .or_else(|| {
            repo.config()
                .agent
                .model
                .clone()
                .and_then(clean_attribution_value)
        });
    match (agent_provider, agent_model) {
        (Some(provider), Some(model)) => {
            let agent = objects::object::Agent::new(provider, model);
            Ok(Attribution::with_agent(principal, agent))
        }
        _ => Ok(Attribution::human(principal)),
    }
}

pub(crate) fn resolve_principal(repo: &Repository, user_config: &UserConfig) -> Result<Principal> {
    // Precedence: env > repo .heddle/config.toml > user ~/.config/heddle/config.toml > Unknown.
    //
    // Repo-level config must win over user-level: a repo carrying an
    // explicit `[principal]` is recording "captures in this project use
    // this identity," and the user-level config is the default for
    // repos that DON'T pin one. Inverting that order (the previous
    // implementation) meant every capture in every repo silently
    // adopted the user-level identity, even when the repo had its own
    // `[principal]` declared — regression seen in
    // `e2e::log_never_surfaces_unknown_principal_after_init` where a
    // repo-level "Adam" was overridden by a stray user-level "test".
    if let Some(principal) = Principal::from_env() {
        return Ok(principal);
    }
    if let Some(config) = &repo.config().principal {
        return Ok(Principal::new(&config.name, &config.email));
    }
    if let Some(config) = &user_config.principal {
        return Ok(Principal::new(&config.name, &config.email));
    }
    Ok(repo.get_principal()?)
}

/// Walks the `anyhow::Error` source chain looking for an underlying
/// `io::Error` that [`objects::fs_atomic::is_out_of_space`] classifies
/// as ENOSPC. We can't pattern-match on `HeddleError::Io(_)` directly
/// because the snapshot path returns `anyhow::Error` (the same
/// underlying io::Error gets wrapped through `repo::HeddleError` and
/// then `From<HeddleError> for anyhow::Error`). Walking `.chain()`
/// finds it regardless of intermediate wrapping.
fn is_disk_full_anyhow(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(objects::fs_atomic::is_out_of_space)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_disk_full_anyhow_detects_direct_io_error() {
        let io_err = std::io::Error::from_raw_os_error(28);
        let any: anyhow::Error = io_err.into();
        assert!(is_disk_full_anyhow(&any));
    }

    #[test]
    fn is_disk_full_anyhow_detects_storage_full_kind() {
        let io_err = std::io::Error::new(
            std::io::ErrorKind::StorageFull,
            "out of disk space writing /tmp/x: free disk space and re-run the command",
        );
        let any: anyhow::Error = io_err.into();
        assert!(is_disk_full_anyhow(&any));
    }

    #[test]
    fn is_disk_full_anyhow_detects_through_anyhow_context() {
        let io_err = std::io::Error::from_raw_os_error(28);
        let wrapped = anyhow::Error::from(io_err).context("snapshot blob write failed");
        assert!(is_disk_full_anyhow(&wrapped));
    }

    #[test]
    fn is_disk_full_anyhow_rejects_unrelated_errors() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope");
        let any: anyhow::Error = io_err.into();
        assert!(!is_disk_full_anyhow(&any));

        let bare = anyhow::anyhow!("something else went wrong");
        assert!(!is_disk_full_anyhow(&bare));
    }

    #[test]
    fn clean_attribution_strips_unknown_placeholder() {
        assert_eq!(clean_attribution_value("unknown".into()), None);
        assert_eq!(clean_attribution_value("Unknown".into()), None);
        assert_eq!(clean_attribution_value("UNKNOWN".into()), None);
        // Trim-then-compare: the harness writes the bare token but
        // belt-and-braces against accidental whitespace.
        assert_eq!(clean_attribution_value("  unknown  ".into()), None);
    }

    #[test]
    fn clean_attribution_strips_empty_and_whitespace() {
        assert_eq!(clean_attribution_value(String::new()), None);
        assert_eq!(clean_attribution_value("   ".into()), None);
        assert_eq!(clean_attribution_value("\t\n".into()), None);
    }

    #[test]
    fn clean_attribution_preserves_real_values() {
        // Real provider/model strings must round-trip with their
        // original casing and surrounding characters intact — the
        // attribution graph keys on these literally.
        assert_eq!(
            clean_attribution_value("anthropic".into()),
            Some("anthropic".into())
        );
        assert_eq!(
            clean_attribution_value("claude-opus-4-7".into()),
            Some("claude-opus-4-7".into())
        );
        // "unknown" as a substring of a real value must not match.
        assert_eq!(
            clean_attribution_value("unknown-model-v2".into()),
            Some("unknown-model-v2".into())
        );
    }
}
