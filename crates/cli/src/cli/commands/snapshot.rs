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

use super::{
    action_line::print_next,
    advice::RecoveryAdvice,
    command_catalog::ActionTemplate,
    error_envelope::print_error_with_hint,
    git_overlay_health::{
        GitOverlayMutationPreflight, RepositoryVerificationState, action_template,
        build_repository_verification_state, git_overlay_mutation_preflight_advice,
        plain_git_mutation_preflight_advice, unimported_git_history_advice,
    },
    next_action::{NextActionValidationContext, write_command_json},
    operator_core::complete_current_thread_manual_resolution,
    thread::find_active_thread_entry,
    thread_cmd::current_thread,
};
use crate::{
    attribution::clean_attribution_value,
    bridge::GitBridge,
    cli::{Cli, output_is_compact, should_output_json, style},
    config::UserConfig,
};

#[derive(Serialize)]
pub(crate) struct SnapshotOutput {
    pub output_kind: &'static str,
    pub status: &'static str,
    pub action: &'static str,
    pub change_id: String,
    pub content_hash: String,
    pub intent: Option<String>,
    pub confidence: Option<f32>,
    pub principal: SnapshotPrincipalOutput,
    pub agent: Option<SnapshotAgentOutput>,
    pub promotion_suggested: bool,
    pub heavy_impact_paths: Vec<String>,
    /// Whether this state carries an ed25519 author signature (heddle#482).
    /// `false` means signing degraded (no key, or an unreadable key); the
    /// state is still captured, just unsigned — surfaced here so a degraded
    /// signing path is never silent.
    pub signed: bool,
    pub message: String,
    pub next_action: Option<String>,
    pub next_action_template: Option<ActionTemplate>,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplate>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
}

impl super::compact::CompactProjection for SnapshotOutput {
    fn compact(&self) -> super::compact::CompactOutput {
        let mut compact = super::compact::CompactOutput::new(self.output_kind);
        compact.status = Some(self.status.to_string());
        let action = self
            .recommended_action
            .as_ref()
            .filter(|action| !action.trim().is_empty())
            .map(|action| (action, &self.recommended_action_template))
            .or_else(|| {
                self.next_action
                    .as_ref()
                    .filter(|action| !action.trim().is_empty())
                    .map(|action| (action, &self.next_action_template))
            });
        if let Some((action, template)) = action {
            compact.next_action = Some(action.clone());
            compact.next_action_template = template.clone();
        }
        compact
    }
}

#[derive(Serialize)]
pub(crate) struct SnapshotPrincipalOutput {
    name: String,
    email: String,
}

#[derive(Serialize)]
pub(crate) struct SnapshotAgentOutput {
    provider: String,
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    segment_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    policy_id: Option<String>,
}

impl From<&Principal> for SnapshotPrincipalOutput {
    fn from(principal: &Principal) -> Self {
        Self {
            name: principal.name.clone(),
            email: principal.email.clone(),
        }
    }
}

impl From<&Agent> for SnapshotAgentOutput {
    fn from(agent: &Agent) -> Self {
        Self {
            provider: agent.provider.clone(),
            model: agent.model.clone(),
            session_id: agent.session_id.clone(),
            segment_id: agent.segment_id.clone(),
            policy_id: agent.policy_id.clone(),
        }
    }
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

#[derive(Clone, Debug, Default)]
struct AgentEnv {
    provider: Option<String>,
    model: Option<String>,
    policy: Option<String>,
    session: Option<String>,
    segment: Option<String>,
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
    let intent = require_capture_intent(intent)?;
    let cwd;
    let start = if let Some(path) = cli.repo.as_ref() {
        path
    } else {
        cwd = std::env::current_dir()?;
        &cwd
    };
    if let Some(advice) = plain_git_mutation_preflight_advice(start, "capture")? {
        return Err(anyhow!(advice));
    }

    let repo = Repository::open(start)?;
    let user_config = UserConfig::load_default().unwrap_or_default();

    if let Some(advice) = unimported_git_history_advice(&repo, "capture")? {
        return Err(anyhow!(advice));
    }
    preflight_large_capture(&repo, force)?;
    let complete_thread_resolution =
        repo.merge_state_manager()
            .load()?
            .is_some_and(|merge_state| {
                merge_state
                    .conflicts
                    .iter()
                    .all(|path| merge_state.resolved.contains(path))
            });
    let mut output = match create_snapshot(&repo, &user_config, Some(intent), confidence, agent) {
        Ok(output) => output,
        Err(err) => {
            // ENOSPC is the only mid-capture failure where the user's
            // working tree is guaranteed safe (we never touched it) and
            // the recovery is mechanical (free disk, re-run). Surface
            // that contract through the shared advice/envelope renderer
            // while preserving the stable exit code. Every other error
            // bubbles through `?` unchanged so the existing diagnostics
            // path keeps working.
            if is_disk_full_anyhow(&err) {
                let err = anyhow!(capture_disk_full_advice(&err));
                print_error_with_hint(cli, &err);
                std::process::exit(CAPTURE_EXIT_DISK_FULL);
            }
            return Err(err);
        }
    };
    if complete_thread_resolution
        && let Some(next_action) = complete_current_thread_manual_resolution(&repo)?
    {
        output.next_action = Some(next_action.clone());
        output.next_action_template = action_template(&next_action);
        output.recommended_action = Some(next_action.clone());
        output.recommended_action_template = action_template(&next_action);
    }

    let as_json = should_output_json(cli, Some(repo.config()));
    let git_overlay = repo.capability() == repo::RepositoryCapability::GitOverlay;

    // In a colocated checkout, mark newly-captured files as intent-to-add
    // in the real `.git/index` so `git status` shows `AM` ("Heddle knows
    // about it") instead of `??` ("untracked"). Best-effort: the state is
    // already durably captured, so a presentation-only index update must
    // not fail the command. `capture` is a Heddle parent/state change —
    // the call frequency jj's `update_intent_to_add` is designed for.
    if git_overlay {
        match repo.current_state() {
            Ok(Some(state)) => {
                let bridge = GitBridge::new(&repo);
                if let Err(err) = bridge.update_intent_to_add(&state.change_id) {
                    debug!("intent-to-add index update skipped: {err}");
                }
            }
            Ok(None) => {}
            Err(err) => debug!("intent-to-add index update skipped: {err}"),
        }
    }

    if as_json {
        write_command_json(
            &output,
            output_is_compact(cli),
            NextActionValidationContext::new(&["capture"], repo.capability()),
        )?;
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
        println!(
            "Saved by: {}",
            style::principal(&output.principal.name, &output.principal.email)
        );
        if let Some(agent) = &output.agent {
            println!(
                "Agent: {}/{}",
                style::bold(&agent.provider),
                style::dim(&agent.model)
            );
        }
        if !output.signed {
            // Degraded signing must be visible, never silent (heddle#482).
            println!(
                "{}",
                style::warn(
                    "Unsigned: no signing identity available — captured without an ed25519 signature"
                )
            );
        }
        if output.confidence.is_some() {
            let confidence_text = format_confidence(output.confidence);
            println!(
                "Confidence: {}",
                style::confidence(output.confidence, &confidence_text)
            );
        }
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
        if let Some(next) = output.recommended_action.as_deref() {
            print_next(next);
        } else if !git_overlay
            && let Ok(Some(thread)) = current_thread(&repo)
            && thread.target_thread.is_some()
        {
            print_next("heddle ready");
        }
    }

    let captured_thread_targets_integration = current_thread(&repo)
        .ok()
        .flatten()
        .and_then(|thread| thread.target_thread)
        .is_some();
    if !git_overlay && !captured_thread_targets_integration {
        // Discoverability tip after a successful capture in native Heddle
        // repos. In Git-overlay repos the concrete checkpoint next step
        // above is more useful; in isolated feature checkouts, `ready`
        // is the next product step.
        crate::cli::tips::maybe_emit(
            repo.root(),
            Some(repo.config()),
            crate::cli::tips::Tip::CheckpointAfterCapture,
            as_json,
            cli.quiet,
        );
    }

    Ok(())
}

pub(crate) fn require_capture_intent(intent: Option<String>) -> Result<String> {
    match intent {
        Some(intent) if !intent.trim().is_empty() => Ok(intent),
        _ => Err(anyhow!(missing_capture_intent_advice())),
    }
}

fn missing_capture_intent_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "missing_capture_intent",
        "refusing to capture without an intent",
        "Provide a short intent with `heddle capture -m \"...\"`.",
        "no capture intent was supplied with -m/--message/--intent",
        "capturing without intent would create a weak provenance record",
        "repository state, refs, metadata, and worktree files were left unchanged",
        "heddle capture -m \"...\"",
        vec!["heddle capture -m \"...\"".to_string()],
    )
}

fn missing_capture_identity_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "capture_identity_required",
        "Refusing to capture: no accountable identity is configured",
        "Set `HEDDLE_PRINCIPAL_NAME` and `HEDDLE_PRINCIPAL_EMAIL`, or run `heddle init --principal-name <name> --principal-email <email>`, then retry the capture.",
        "Heddle would otherwise have to record Unknown <unknown@example.com> on the captured state",
        "capture would create durable Heddle history without a real principal",
        "Heddle refs, captured states, Git refs, index, and worktree files were left unchanged",
        "heddle init --principal-name <name> --principal-email <email>",
        vec![
            "heddle init --principal-name <name> --principal-email <email>".to_string(),
            "heddle capture -m \"...\"".to_string(),
        ],
    )
}

/// Resolve the current thread name for hook payloads. Returns `""`
/// when HEAD is detached (no thread context); the hook protocol uses
/// the same empty-string sentinel.
fn current_thread_name(repo: &Repository) -> String {
    use refs::Head;
    match repo.head_ref() {
        Ok(Head::Attached { thread }) => thread.to_string(),
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
    Err(anyhow!(large_capture_advice(
        total,
        delete_count,
        add_count,
        sample,
    )))
}

fn large_capture_advice(
    total: usize,
    delete_count: usize,
    add_count: usize,
    sample: String,
) -> RecoveryAdvice {
    let sample = if sample.is_empty() {
        "no sample paths available".to_string()
    } else {
        sample
    };
    RecoveryAdvice::safety_refusal(
        "large_capture_requires_force",
        format!(
            "Large capture safety check: this would capture {total} changed paths ({delete_count} deletions, {add_count} additions)"
        ),
        "If this is intentional, rerun with `heddle capture --force -m \"...\"`.",
        format!("sample changed paths: {sample}"),
        "capture would preserve an unusually large Git-overlay worktree change without an explicit confirmation",
        "repository state, refs, metadata, and worktree files were left unchanged",
        "heddle capture --force -m \"...\"",
        vec!["heddle capture --force -m \"...\"".to_string()],
    )
}

fn capture_disk_full_advice(err: &anyhow::Error) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "capture_out_of_space",
        format!("Capture aborted because the filesystem is out of space: {err:#}"),
        "Free disk space and re-run `heddle capture`. Your working tree changes are intact.",
        "the filesystem reported no remaining space while Heddle was writing captured objects",
        "retrying before freeing space may fail again or leave another incomplete object write",
        "the working tree was not modified; already-committed repository data remains behind atomic write boundaries",
        "heddle capture -m \"...\"",
        vec!["heddle capture -m \"...\"".to_string()],
    )
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

pub(crate) fn create_snapshot_from_tree(
    repo: &Repository,
    user_config: &UserConfig,
    tree: Tree,
    intent: Option<String>,
    confidence: Option<f32>,
    agent: SnapshotAgentOverrides,
) -> Result<SnapshotOutput> {
    create_snapshot_from_tree_profiled(repo, user_config, tree, intent, confidence, agent)
        .map(|(output, _)| output)
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

    if let Some(advice) = git_overlay_mutation_preflight_advice(
        repo,
        "capture",
        GitOverlayMutationPreflight::capture_like(),
    )? {
        return Err(anyhow!(advice));
    }

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

    // Invariant A (heddle#317, spike #266 §5.4): the inherited default
    // visibility tier is bound to the freshly created state inside the snapshot
    // chokepoint (`snapshot_with_attribution_profiled`), so every creator —
    // capture, cherry-pick, revert, daemon/mount — inherits it by construction.
    // No per-call-site bind here.
    let mut execution =
        repo.snapshot_with_attribution_profiled(intent.clone(), confidence, attribution)?;

    let thread_metadata_start = Instant::now();
    let (promotion_suggested, heavy_impact_paths) =
        update_active_thread_metadata(repo, &execution.state, &execution.tree)?;
    let thread_metadata_ms = thread_metadata_start.elapsed().as_millis();

    let trust = build_repository_verification_state(repo);
    let recommended_action =
        (!trust.recommended_action.trim().is_empty()).then(|| trust.recommended_action.clone());
    let recommended_action_template = recommended_action
        .as_deref()
        .and_then(action_template)
        .or_else(|| trust.recommended_action_template.clone());

    let output = SnapshotOutput {
        output_kind: "capture",
        status: "captured",
        action: "capture",
        change_id: execution.state.change_id.short(),
        content_hash: execution.state.hash().short(),
        intent: execution.state.intent.clone(),
        confidence: execution.state.confidence,
        principal: (&execution.state.attribution.principal).into(),
        agent: execution
            .state
            .attribution
            .agent
            .as_ref()
            .map(SnapshotAgentOutput::from),
        promotion_suggested,
        heavy_impact_paths: heavy_impact_paths.clone(),
        signed: execution.state.signature.is_some(),
        message: format!(
            "Captured state {} ({})",
            execution.state.change_id.short(),
            execution.state.hash().short()
        ),
        next_action: recommended_action.clone(),
        next_action_template: recommended_action_template.clone(),
        recommended_action,
        recommended_action_template,
        trust,
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

pub(crate) fn create_snapshot_from_tree_profiled(
    repo: &Repository,
    user_config: &UserConfig,
    tree: Tree,
    intent: Option<String>,
    confidence: Option<f32>,
    agent: SnapshotAgentOverrides,
) -> Result<(SnapshotOutput, SnapshotCommandProfile)> {
    info!("Creating snapshot from supplied tree");

    if let Some(advice) = git_overlay_mutation_preflight_advice(
        repo,
        "capture",
        GitOverlayMutationPreflight::capture_like(),
    )? {
        return Err(anyhow!(advice));
    }

    let hook_manager = HookManager::new(repo);
    let hook_ctx = HookContext::new(repo);

    hook_manager.run(Hook::PreSnapshot, &hook_ctx)?;

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

    let mut execution = repo.snapshot_tree_with_attribution_profiled(
        tree,
        intent.clone(),
        confidence,
        attribution,
    )?;
    let thread_metadata_start = Instant::now();
    let (promotion_suggested, heavy_impact_paths) =
        update_active_thread_metadata(repo, &execution.state, &execution.tree)?;
    let thread_metadata_ms = thread_metadata_start.elapsed().as_millis();

    let trust = build_repository_verification_state(repo);
    let recommended_action =
        (!trust.recommended_action.trim().is_empty()).then(|| trust.recommended_action.clone());
    let recommended_action_template = recommended_action
        .as_deref()
        .and_then(action_template)
        .or_else(|| trust.recommended_action_template.clone());

    let output = SnapshotOutput {
        output_kind: "capture",
        status: "captured",
        action: "capture",
        change_id: execution.state.change_id.short(),
        content_hash: execution.state.hash().short(),
        intent: execution.state.intent.clone(),
        confidence: execution.state.confidence,
        principal: (&execution.state.attribution.principal).into(),
        agent: execution
            .state
            .attribution
            .agent
            .as_ref()
            .map(SnapshotAgentOutput::from),
        promotion_suggested,
        heavy_impact_paths: heavy_impact_paths.clone(),
        signed: execution.state.signature.is_some(),
        message: format!(
            "Captured state {} ({})",
            execution.state.change_id.short(),
            execution.state.hash().short()
        ),
        next_action: recommended_action.clone(),
        next_action_template: recommended_action_template.clone(),
        recommended_action,
        recommended_action_template,
        trust,
    };

    hook_manager.run(Hook::PostSnapshot, &hook_ctx)?;

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
    build_attribution_with_env(repo, user_config, agent, current_agent_env())
}

fn current_agent_env() -> AgentEnv {
    AgentEnv {
        provider: std::env::var("HEDDLE_AGENT_PROVIDER")
            .ok()
            .and_then(clean_attribution_value),
        model: std::env::var("HEDDLE_AGENT_MODEL")
            .ok()
            .and_then(clean_attribution_value),
        policy: std::env::var("HEDDLE_AGENT_POLICY")
            .ok()
            .and_then(clean_attribution_value),
        session: std::env::var("HEDDLE_SESSION_ID").ok(),
        segment: std::env::var("HEDDLE_SESSION_SEGMENT").ok(),
    }
}

fn build_attribution_with_env(
    repo: &Repository,
    user_config: &UserConfig,
    agent: &SnapshotAgentOverrides,
    env: AgentEnv,
) -> Result<Attribution> {
    let principal = resolve_principal(repo, user_config)?;
    if is_default_unknown_principal(&principal) {
        return Err(anyhow!(missing_capture_identity_advice()));
    }

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
    // provenance demo and the `heddle query --attribution --context` story.
    //
    // Precedence: explicit CLI overrides and `HEDDLE_AGENT_*` env are
    // user-supplied attribution for this capture, so they must not be
    // silently masked by a detected harness actor. The active thread
    // actor remains the zero-config fallback for agent threads when no
    // explicit attribution is present.
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
    let session_provider = current_session
        .as_ref()
        .and_then(|session| session.current_segment())
        .map(|segment| segment.provider.clone())
        .and_then(clean_attribution_value);
    let session_model = current_session
        .as_ref()
        .and_then(|session| session.current_segment())
        .map(|segment| segment.model.clone())
        .and_then(clean_attribution_value);
    let session_policy = current_session
        .as_ref()
        .and_then(|session| session.current_segment())
        .and_then(|segment| segment.policy_id.clone())
        .and_then(clean_attribution_value);
    let harness_probe = crate::harness::probe_current_process_harness(
        repo,
        thread_provider.clone().or_else(|| session_provider.clone()),
        thread_model.clone().or_else(|| session_model.clone()),
        session_policy.clone(),
    )
    .ok();
    let harness_provider = harness_probe
        .as_ref()
        .and_then(|probe| probe.provider.clone())
        .and_then(clean_attribution_value);
    let harness_model = harness_probe
        .as_ref()
        .and_then(|probe| probe.model.clone())
        .and_then(clean_attribution_value);
    let harness_policy = harness_probe
        .as_ref()
        .and_then(|probe| probe.policy.clone())
        .and_then(clean_attribution_value);

    let provider = agent
        .provider
        .clone()
        .or(env.provider)
        .or(thread_provider)
        .or(harness_provider)
        .or(session_provider)
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
    let model = agent
        .model
        .clone()
        .or(env.model)
        .or(thread_model)
        .or(harness_model)
        .or(session_model)
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
    let session_id = agent
        .session
        .clone()
        .or(env.session)
        .or_else(|| current_session.as_ref().map(|session| session.id.clone()));
    let segment_id = agent.segment.clone().or(env.segment).or_else(|| {
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
            .or(env.policy)
            .or(harness_policy)
            .or(session_policy)
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
    let harness_probe = crate::harness::probe_current_process_harness(repo, None, None, None).ok();
    let harness_provider = harness_probe
        .as_ref()
        .and_then(|probe| probe.provider.clone())
        .and_then(clean_attribution_value);
    let harness_model = harness_probe
        .as_ref()
        .and_then(|probe| probe.model.clone())
        .and_then(clean_attribution_value);
    let agent_provider = std::env::var("HEDDLE_AGENT_PROVIDER")
        .ok()
        .and_then(clean_attribution_value)
        .or(harness_provider)
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
        .or(harness_model)
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
    // Precedence: env > repo .heddle/config.toml > Git-overlay Git config
    // (including the shared parent checkout for isolated work) > user
    // ~/.config/heddle/config.toml > Unknown.
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
    let principal = repo.get_principal()?;
    if !is_default_unknown_principal(&principal) {
        return Ok(principal);
    }
    if let Some(config) = &user_config.principal {
        return Ok(Principal::new(&config.name, &config.email));
    }
    Ok(principal)
}

pub(crate) fn is_placeholder_principal(principal: &Principal) -> bool {
    let name = principal.name.trim();
    let email = principal.email.trim().to_ascii_lowercase();
    name.is_empty()
        || email.is_empty()
        || (name == "T" && email == "t@e.c")
        || email.ends_with("@e.c")
}

pub(crate) fn placeholder_principal_warning(principal: &Principal) -> String {
    format!(
        "WARNING: principal attribution looks like a placeholder: {principal}. Set a real identity with `heddle init --principal-name <name> --principal-email <email>`."
    )
}

fn is_default_unknown_principal(principal: &Principal) -> bool {
    principal.name.trim().is_empty()
        || principal.email.trim().is_empty()
        || (principal.name.trim() == "Unknown" && principal.email.trim() == "unknown@example.com")
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

    fn user_config_with_principal() -> UserConfig {
        UserConfig {
            principal: Some(crate::config::UserPrincipalConfig {
                name: "Ada Lovelace".to_string(),
                email: "ada@example.com".to_string(),
            }),
            ..UserConfig::default()
        }
    }

    fn save_active_harness_entry(
        repo: &Repository,
        provider: &str,
        model: &str,
    ) -> objects::store::AgentEntry {
        let thread = current_thread(repo)
            .unwrap()
            .expect("initialized repository has a current thread");
        let registry = objects::store::AgentRegistry::new(repo.heddle_dir());
        let entry = objects::store::AgentEntry {
            session_id: objects::store::generate_agent_id(),
            client_instance_id: None,
            native_actor_key: Some("claude-code:session:session-457".to_string()),
            native_parent_actor_key: None,
            native_instance_key: Some("claude-code:transcript:/tmp/claude/457.jsonl".to_string()),
            heddle_session_id: None,
            thread_id: Some(thread.id.clone()),
            thread: thread.id,
            pid: Some(std::process::id()),
            boot_id: None,
            liveness_path: None,
            heartbeat_at: Some(chrono::Utc::now()),
            anchor_state: None,
            anchor_root: None,
            reservation_token: Some(objects::store::generate_agent_id()),
            path: Some(repo.root().to_path_buf()),
            base_state: String::new(),
            started_at: chrono::Utc::now(),
            provider: Some(provider.to_string()),
            model: Some(model.to_string()),
            harness: Some("claude-code".to_string()),
            thinking_level: None,
            usage_summary: objects::store::AgentUsageSummary::default(),
            last_progress_at: None,
            report_flush_state: Some("pending-local".to_string()),
            attach_reason: Some("test detected harness actor".to_string()),
            attach_precedence: Vec::new(),
            winning_attach_rule: Some("test".to_string()),
            probe_source: Some("hook_payload".to_string()),
            probe_confidence: Some(0.99),
            status: objects::store::AgentStatus::Active,
            completed_at: None,
            context_queries: Vec::new(),
        };
        registry.save(&entry).unwrap();
        entry
    }

    fn empty_agent_overrides() -> SnapshotAgentOverrides {
        SnapshotAgentOverrides {
            provider: None,
            model: None,
            session: None,
            segment: None,
            policy: None,
            no_policy: false,
            no_agent: false,
        }
    }

    #[test]
    fn build_attribution_explicit_env_wins_over_active_harness_actor() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        save_active_harness_entry(&repo, "anthropic", "claude-opus-4-8[1m]");

        let attribution = build_attribution_with_env(
            &repo,
            &user_config_with_principal(),
            &empty_agent_overrides(),
            AgentEnv {
                provider: Some("openai".to_string()),
                model: Some("gpt-5-codex".to_string()),
                ..AgentEnv::default()
            },
        )
        .unwrap();

        let agent = attribution.agent.expect("explicit env should set agent");
        assert_eq!(agent.provider, "openai");
        assert_eq!(agent.model, "gpt-5-codex");
    }

    #[test]
    fn build_attribution_uses_detected_harness_actor_when_env_absent() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        save_active_harness_entry(&repo, "anthropic", "claude-opus-4-8[1m]");

        let attribution = build_attribution_with_env(
            &repo,
            &user_config_with_principal(),
            &empty_agent_overrides(),
            AgentEnv::default(),
        )
        .unwrap();

        let agent = attribution
            .agent
            .expect("detected harness actor should set agent");
        assert_eq!(agent.provider, "anthropic");
        assert_eq!(agent.model, "claude-opus-4-8[1m]");
    }

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
    fn capture_disk_full_advice_preserves_capture_contract() {
        let io_err = std::io::Error::from_raw_os_error(28);
        let err = anyhow::Error::from(io_err).context("snapshot blob write failed");
        let advice = capture_disk_full_advice(&err);

        assert_eq!(advice.kind, "capture_out_of_space");
        assert!(advice.error.contains("Capture aborted"));
        assert!(advice.hint.contains("heddle capture"));
        assert!(advice.hint.contains("working tree changes are intact"));
        assert_eq!(advice.primary_command, "heddle capture -m \"...\"");
        assert_eq!(
            advice.recovery_commands,
            vec!["heddle capture -m \"...\"".to_string()]
        );
        assert!(advice.preserved.contains("working tree was not modified"));
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
