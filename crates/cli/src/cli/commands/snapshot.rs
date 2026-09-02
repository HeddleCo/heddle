// SPDX-License-Identifier: Apache-2.0
//! Snapshot command.

use std::time::Instant;

use anyhow::{Result, anyhow};
// The wire payloads live in cli-contract so the schema registry registers
// the real serialization types; the compact projection stays with the CLI.
pub(crate) use heddle_cli_contract::cli::commands::wire::{
    SnapshotAgentOutput, SnapshotOutput, SnapshotPrincipalOutput,
};
use hosted_client::attribution::clean_attribution_value;
use objects::{
    lock::RepositoryLockExt,
    object::{Agent, Attribution, Principal, StateId, ThreadName, Tree},
    store::ObjectStore,
};
use refs::Head;
use repo::{Repository, RepositoryCapability, SessionManager, SnapshotProfile, format_confidence};
// Re-export the helper derivations so existing CLI call sites
// (`thread.rs`; the agent relay reads them from `repo` directly) keep
// `super::snapshot::summarize_*`
// imports working without churn. The implementations live in
// `repo::snapshot_metadata` so the mount and CLI paths share the same
// logic.
pub(crate) use repo::{summarize_confidence, summarize_verification};
use serde::Serialize;
use tracing::{debug, info};
use verbs::{
    CaptureOptions, GitScope, MachineContractInput, SavePlan, SaveVerb, execute_save,
    principal_lacks_accountable_identity,
};

use super::{
    action_line::print_next,
    advice::RecoveryAdvice,
    next_action::{NextActionValidationContext, write_command_json},
    thread::find_active_thread_entry,
    thread_cmd::current_thread,
    verification_health::{
        GitOverlayMutationPreflight, action_template, git_overlay_mutation_preflight_advice,
        machine_contract_coverage, plain_git_mutation_preflight_advice,
    },
};
use crate::{
    cli::{
        Cli, execution_context_from_cli_parts, output_is_compact, should_output_json, style,
        worktree_status_options,
    },
    config::UserConfig,
    perf::{ProfileField, emit_profile, instrumentation_enabled},
};

impl super::compact::CompactProjection for SnapshotOutput {
    fn compact(&self) -> super::compact::CompactOutput {
        let mut compact = super::compact::CompactOutput::new(self.output_kind);
        compact.status = Some(self.status.to_string());
        compact.state_id = Some(self.state_id.clone());
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

#[derive(Clone, Debug, Default, Serialize)]
pub struct SnapshotCommandProfile {
    pub tree_walk_ms: u128,
    pub blob_prep_ms: u128,
    pub blob_write_ms: u128,
    pub tree_write_ms: u128,
    pub state_ref_oplog_ms: u128,
    pub atomic_execute_ms: u128,
    pub ref_publish_ms: u128,
    pub state_create_ms: u128,
    pub captured_path_count_ms: u128,
    pub post_verification_ms: u128,
    pub thread_metadata_ms: u128,
    pub output_build_ms: u128,
    pub output_task_assignment_ms: u128,
    pub output_principal_ms: u128,
    pub preflight_ms: u128,
    pub attribution_ms: u128,
    pub execute_save_ms: u128,
    pub previous_state_ms: u128,
    pub previous_state_head_ms: u128,
    pub previous_state_cache_read_ms: u128,
    pub previous_state_cache_decode_ms: u128,
    pub previous_state_cache_validate_ms: u128,
    pub previous_state_store_read_ms: u128,
    pub previous_state_cache_hit: bool,
    pub signature_lookup_ms: u128,
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

pub fn cmd_snapshot(
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
    let user_config = UserConfig::load_default()?;

    let status_options = worktree_status_options(Some(repo.config()));
    let ctx = execution_context_from_cli_parts(cli, start, Some(repo), &user_config)?;
    let snapshot_start = Instant::now();
    let capture_report = verbs::capture(
        &ctx,
        CaptureOptions {
            intent,
            confidence,
            force,
            worktree_status_options: status_options,
            machine_contract_input: Some(MachineContractInput::from_coverage(
                machine_contract_coverage(),
            )),
        },
        |repo| build_capture_attribution(repo, &user_config, &agent),
    )?;
    let snapshot_ms = snapshot_start.elapsed().as_millis();
    let worktree_status_ms = capture_report.diagnostics.profile.worktree_status_ms;
    let captured_thread_targets_integration = capture_report.captured_thread_targets_integration;
    let (output, snapshot_profile) = snapshot_output_from_capture_report(capture_report);
    let repo = ctx.require_repo()?;
    super::automatic_repack::note_committed_capture(repo);

    let as_json = should_output_json(cli, Some(repo.config()));
    let git_overlay = repo.capability() == repo::RepositoryCapability::GitOverlay;

    let render_start = Instant::now();
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
            style::state_id(&output.state_id),
            style::dim(&output.content_hash),
        );
        println!(
            "Captured by: {} from {}",
            style::principal(&output.principal.name, &output.principal.email),
            verbs::principal_source_display(&output.principal_source)
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
        for warning in &output.warnings {
            println!("{}", style::warn(&format!("Warning: {warning}")));
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
        } else if !git_overlay && captured_thread_targets_integration {
            print_next("heddle ready");
        }
    }
    let render_ms = render_start.elapsed().as_millis();

    if git_overlay && !captured_thread_targets_integration {
        // Overlay-only discoverability: commit writes the captured tree
        // to `.git`. Native capture is already the save boundary.
        crate::cli::tips::maybe_emit(
            repo.root(),
            Some(repo.config()),
            crate::cli::tips::Tip::CheckpointAfterCapture,
            as_json,
            cli.quiet,
        );
    }

    if instrumentation_enabled() {
        emit_profile(
            "capture phases",
            &[
                // Pre-mutation git-overlay worktree status walk (threaded into
                // both the large-capture preflight and the snapshot mutation
                // preflight — a single walk for both).
                ProfileField::millis("worktree_status_ms", worktree_status_ms),
                // The snapshot itself (preflight + tree build + blob/tree write
                // + state/ref/oplog), broken down below.
                ProfileField::millis("snapshot_ms", snapshot_ms),
                ProfileField::millis("snapshot_tree_walk_ms", snapshot_profile.tree_walk_ms),
                ProfileField::millis("snapshot_blob_prep_ms", snapshot_profile.blob_prep_ms),
                ProfileField::millis("snapshot_blob_write_ms", snapshot_profile.blob_write_ms),
                ProfileField::millis("snapshot_tree_write_ms", snapshot_profile.tree_write_ms),
                ProfileField::millis(
                    "snapshot_state_ref_oplog_ms",
                    snapshot_profile.state_ref_oplog_ms,
                ),
                ProfileField::millis(
                    "snapshot_atomic_execute_ms",
                    snapshot_profile.atomic_execute_ms,
                ),
                ProfileField::millis("snapshot_ref_publish_ms", snapshot_profile.ref_publish_ms),
                ProfileField::millis("snapshot_state_create_ms", snapshot_profile.state_create_ms),
                ProfileField::millis(
                    "snapshot_captured_path_count_ms",
                    snapshot_profile.captured_path_count_ms,
                ),
                ProfileField::millis(
                    "snapshot_post_verification_ms",
                    snapshot_profile.post_verification_ms,
                ),
                ProfileField::millis(
                    "snapshot_thread_metadata_ms",
                    snapshot_profile.thread_metadata_ms,
                ),
                ProfileField::millis("snapshot_output_build_ms", snapshot_profile.output_build_ms),
                ProfileField::millis(
                    "snapshot_output_task_assignment_ms",
                    snapshot_profile.output_task_assignment_ms,
                ),
                ProfileField::millis(
                    "snapshot_output_principal_ms",
                    snapshot_profile.output_principal_ms,
                ),
                ProfileField::millis("snapshot_preflight_ms", snapshot_profile.preflight_ms),
                ProfileField::millis("snapshot_attribution_ms", snapshot_profile.attribution_ms),
                ProfileField::millis("snapshot_execute_save_ms", snapshot_profile.execute_save_ms),
                ProfileField::millis(
                    "snapshot_previous_state_ms",
                    snapshot_profile.previous_state_ms,
                ),
                ProfileField::millis(
                    "snapshot_previous_state_head_ms",
                    snapshot_profile.previous_state_head_ms,
                ),
                ProfileField::millis(
                    "snapshot_previous_state_cache_read_ms",
                    snapshot_profile.previous_state_cache_read_ms,
                ),
                ProfileField::millis(
                    "snapshot_previous_state_cache_decode_ms",
                    snapshot_profile.previous_state_cache_decode_ms,
                ),
                ProfileField::millis(
                    "snapshot_previous_state_cache_validate_ms",
                    snapshot_profile.previous_state_cache_validate_ms,
                ),
                ProfileField::millis(
                    "snapshot_previous_state_store_read_ms",
                    snapshot_profile.previous_state_store_read_ms,
                ),
                ProfileField::boolean(
                    "snapshot_previous_state_cache_hit",
                    snapshot_profile.previous_state_cache_hit,
                ),
                ProfileField::millis(
                    "snapshot_signature_lookup_ms",
                    snapshot_profile.signature_lookup_ms,
                ),
                ProfileField::millis("render_ms", render_ms),
            ],
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

pub(crate) fn create_snapshot(
    repo: &Repository,
    user_config: &UserConfig,
    intent: Option<String>,
    confidence: Option<f32>,
    agent: SnapshotAgentOverrides,
) -> Result<SnapshotOutput> {
    create_snapshot_profiled(repo, user_config, intent, confidence, agent).map(|(output, _)| output)
}

/// Shared entry for staged-tree captures that still want CLI-shaped
/// [`SnapshotOutput`] (hooks + attribution + execute_save). Kept for
/// non-commit callers; commit now builds a [`SavePlan`] directly.
#[allow(dead_code)]
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
) -> Result<StateId> {
    if let Some(state) = repo.current_state_for_worktree_status()? {
        return Ok(state.state_id);
    }

    // Lazy binding represents the committed Git tip only. A dirty overlay must
    // take the native snapshot path so capture-oriented callers (notably
    // `ready -m`) preserve the worktree bytes they already classified as work.
    if repo.capability() == RepositoryCapability::GitOverlay
        // Preserve the typed corrupt/unreadable-HEAD failure before status
        // inspection; a genuine unborn HEAD still falls through to bootstrap.
        && resolve_active_git_tip_sha(repo)?.is_some()
        && repo
            .git_overlay_worktree_status()?
            .is_some_and(|status| status.is_clean())
        && let Some(state_id) = bind_git_overlay_active_tip(repo)?
    {
        return Ok(state_id);
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

fn bind_git_overlay_active_tip(repo: &Repository) -> Result<Option<StateId>> {
    // Git HEAD is the authority for an unbound overlay. Hold the canonical
    // repository lock from the first authoritative read through sha-map,
    // checkpoint, and Heddle ref publication so concurrent bootstraps cannot
    // publish a stale tip or lose a checkpoint read-modify-write. The lock is
    // same-thread reentrant, so nested repository writers remain deadlock-free.
    let _lock = repo
        .locker()
        .write()
        .map_err(|error| anyhow!("failed to lock repository while binding Git HEAD: {error}"))?;
    let Some(tip_sha) = resolve_active_git_tip_sha(repo)? else {
        return Ok(None);
    };

    if let Some(existing) = repo.git_overlay_mapped_state_for_git_commit(&tip_sha)? {
        if repo.latest_git_checkpoint_for_state(&existing)?.is_none() {
            repo.record_git_checkpoint(
                &existing,
                tip_sha.clone(),
                format!("Bound active Git tip {}", &tip_sha[..tip_sha.len().min(12)]),
            )?;
        }
        point_overlay_head_at_mapped_tip(repo, &existing)?;
        return Ok(Some(existing));
    }

    heddle_git_projection::git_core::GitProjection::hydrate_checkout_heddle_notes_without_mirror(
        repo.root(),
    );
    let state_id = ingest::bind_single_git_commit_overlay(
        repo.root(),
        repo.root(),
        &tip_sha,
        ingest::ImportOptions::default(),
    )
    .map_err(|error| {
        anyhow!(RecoveryAdvice::git_overlay_tip_bind_failed(format!(
            "failed to map Git tip {tip_sha}: {error}"
        )))
    })?;
    if repo.store().get_state(&state_id)?.is_none() {
        return Err(anyhow!(RecoveryAdvice::git_overlay_tip_bind_failed(
            format!(
                "mapped Git tip {tip_sha} to {} but the state object is not readable",
                state_id.short()
            )
        )));
    }
    if repo.latest_git_checkpoint_for_state(&state_id)?.is_none() {
        repo.record_git_checkpoint(
            &state_id,
            tip_sha.clone(),
            format!("Bound active Git tip {}", &tip_sha[..tip_sha.len().min(12)]),
        )?;
    }
    point_overlay_head_at_mapped_tip(repo, &state_id)?;
    info!(
        git_tip = %tip_sha,
        state_id = %state_id.short(),
        "bound active Git tip as first Heddle state"
    );
    Ok(Some(state_id))
}

fn resolve_active_git_tip_sha(repo: &Repository) -> Result<Option<String>> {
    let git = match repo.git_overlay_sley_repository() {
        Ok(Some(git)) => git,
        Ok(None) => {
            return Err(anyhow!(RecoveryAdvice::git_overlay_tip_bind_failed(
                "repository is marked git-overlay but no Git worktree was found"
            )));
        }
        Err(error) => {
            return Err(anyhow!(RecoveryAdvice::git_overlay_tip_bind_failed(
                format!("failed to open Git repository: {error}")
            )));
        }
    };
    match git.head() {
        Ok(head) => Ok(head.oid.map(|oid| oid.to_string())),
        Err(error) => Err(anyhow!(RecoveryAdvice::git_overlay_tip_bind_failed(
            format!("failed to resolve Git HEAD: {error}")
        ))),
    }
}

fn point_overlay_head_at_mapped_tip(repo: &Repository, state_id: &StateId) -> Result<()> {
    // Publication follows Git HEAD's physical attachment state. In
    // particular, a freshly-created sidecar's default attached `main` HEAD
    // must never turn a detached Git commit into a `main` ref update. Check
    // detachment before `git_overlay_current_branch`, whose in-progress
    // fallback can name a branch while Git itself is detached.
    if !repo.git_overlay_head_is_detached()?
        && let Some(branch) = repo.git_overlay_current_branch()?
    {
        let thread = ThreadName::from(branch.as_str());
        if repo.refs().get_thread(&thread)?.as_ref() != Some(state_id) {
            repo.set_thread_recorded(&thread, state_id)?;
        }
        let expected = Head::Attached {
            thread: thread.clone(),
        };
        if repo.refs().read_head()? != expected {
            repo.write_head_recorded(&expected)?;
        }
        return Ok(());
    }

    let expected = Head::Detached { state: *state_id };
    if repo.refs().read_head()? != expected {
        repo.write_head_recorded(&expected)?;
    }
    Ok(())
}

pub(crate) fn create_snapshot_profiled(
    repo: &Repository,
    user_config: &UserConfig,
    intent: Option<String>,
    confidence: Option<f32>,
    agent: SnapshotAgentOverrides,
) -> Result<(SnapshotOutput, SnapshotCommandProfile)> {
    info!("Creating snapshot");

    let preflight_start = Instant::now();
    if let Some(advice) = git_overlay_mutation_preflight_advice(
        repo,
        "capture",
        GitOverlayMutationPreflight::capture_like(),
    )? {
        return Err(anyhow!(advice));
    }
    let preflight_ms = preflight_start.elapsed().as_millis();

    let attribution_start = Instant::now();
    let attribution = build_attribution(repo, user_config, &agent)?;
    let attribution_ms = attribution_start.elapsed().as_millis();
    if let Some(ref agent) = attribution.agent {
        debug!(provider = %agent.provider, model = %agent.model, "Agent attribution");
    }

    // Shared save pipeline: hooks + repo snapshot + thread metadata + verify.
    let plan = SavePlan {
        verb: SaveVerb::Capture,
        intent,
        confidence,
        attribution,
        git_scope: GitScope::None,
        supplied_tree: None,
        reuse_current_state: false,
        require_clean_worktree: false,
        require_worktree_change: repo.capability() == RepositoryCapability::NativeHeddle
            && repo.merge_state_manager().load()?.is_none(),
        worktree_status_options: worktree_status_options(Some(repo.config())),
        known_worktree_changes: None,
        run_hooks: true,
        commit_safe_post_verify: false,
        coalesce_snapshot_and_checkpoint: false,
        linearize_git_parent: false,
        precomputed_worktree_status: None,
        machine_contract_input: Some(MachineContractInput::from_coverage(
            machine_contract_coverage(),
        )),
    };
    let execute_save_start = Instant::now();
    let report = execute_save(repo, plan)?;
    let execute_save_ms = execute_save_start.elapsed().as_millis();
    if report.created_new_state {
        super::automatic_repack::note_committed_capture(repo);
    }
    let (output, mut profile) = snapshot_output_from_save_report(repo, user_config, report)?;
    profile.preflight_ms = preflight_ms;
    profile.attribution_ms = attribution_ms;
    profile.execute_save_ms = execute_save_ms;
    Ok((output, profile))
}

#[allow(dead_code)]
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

    let attribution = build_attribution(repo, user_config, &agent)?;
    if let Some(ref agent) = attribution.agent {
        debug!(provider = %agent.provider, model = %agent.model, "Agent attribution");
    }

    let plan = SavePlan {
        verb: SaveVerb::Capture,
        intent,
        confidence,
        attribution,
        git_scope: GitScope::None,
        supplied_tree: Some(tree),
        reuse_current_state: false,
        require_clean_worktree: false,
        require_worktree_change: false,
        worktree_status_options: worktree_status_options(Some(repo.config())),
        known_worktree_changes: None,
        run_hooks: true,
        commit_safe_post_verify: false,
        coalesce_snapshot_and_checkpoint: false,
        linearize_git_parent: false,
        precomputed_worktree_status: None,
        machine_contract_input: Some(MachineContractInput::from_coverage(
            machine_contract_coverage(),
        )),
    };
    let report = execute_save(repo, plan)?;
    if report.created_new_state {
        super::automatic_repack::note_committed_capture(repo);
    }
    snapshot_output_from_save_report(repo, user_config, report)
}

fn snapshot_output_from_capture_report(
    report: verbs::CaptureReport,
) -> (SnapshotOutput, SnapshotCommandProfile) {
    let output_build_start = Instant::now();
    let verbs::CaptureDiagnostics {
        save,
        profile: capture_profile,
    } = report.diagnostics;
    let previous_state_profile = save.previous_state_profile.clone();
    let next_action = report.recommended_action.clone();
    let next_action_template = report.recommended_action_template.clone();
    let output = SnapshotOutput {
        output_kind: report.output_kind,
        status: "captured",
        action: "capture",
        state_id: report.state_id,
        content_hash: report.content_hash,
        intent: report.intent,
        confidence: report.confidence,
        task_assignment_id: report.task_assignment_id,
        principal: SnapshotPrincipalOutput {
            name: report.principal.name,
            email: report.principal.email,
        },
        principal_source: report.principal_source,
        agent: report.agent.map(|agent| SnapshotAgentOutput {
            provider: agent.provider,
            model: agent.model,
            session_id: agent.session_id,
            segment_id: agent.segment_id,
            policy_id: agent.policy_id,
            thought_level: agent.thought_level,
            parent: agent.parent,
        }),
        promotion_suggested: report.promotion_suggested,
        heavy_impact_paths: report.heavy_impact_paths,
        captured_path_count: report.captured_path_count,
        warnings: report.warnings,
        signed: report.signed,
        message: report.message,
        next_action,
        next_action_template,
        recommended_action: report.recommended_action,
        recommended_action_template: report.recommended_action_template,
        trust: report.verification,
    };
    let mut profile = snapshot_command_profile(
        save.snapshot_profile,
        save.state_create_ms,
        save.captured_path_count_ms,
        save.post_verification_ms,
        save.thread_metadata_ms,
    );
    profile.output_build_ms = output_build_start.elapsed().as_millis();
    profile.preflight_ms = capture_profile.preflight_ms;
    profile.attribution_ms = capture_profile.attribution_ms;
    profile.execute_save_ms = capture_profile.execute_save_ms;
    profile.previous_state_ms = save.previous_state_ms;
    profile.previous_state_head_ms = previous_state_profile.head_ms;
    profile.previous_state_cache_read_ms = previous_state_profile.cache_read_ms;
    profile.previous_state_cache_decode_ms = previous_state_profile.cache_decode_ms;
    profile.previous_state_cache_validate_ms = previous_state_profile.cache_validate_ms;
    profile.previous_state_store_read_ms = previous_state_profile.store_read_ms;
    profile.previous_state_cache_hit = previous_state_profile.cache_hit;
    profile.signature_lookup_ms = save.signature_lookup_ms;
    (output, profile)
}

fn snapshot_output_from_save_report(
    repo: &Repository,
    user_config: &UserConfig,
    report: verbs::SaveReport,
) -> Result<(SnapshotOutput, SnapshotCommandProfile)> {
    let output_build_start = Instant::now();
    let previous_state_ms = report.previous_state_ms;
    let previous_state_profile = report.previous_state_profile.clone();
    let signature_lookup_ms = report.signature_lookup_ms;
    // Public capture JSON still uses the CLI verification adapter so
    // Machine-Contract Proof is injected from the command catalog. Core
    // `execute_save` already computed proof for the embedder path.
    let trust = report.verification.clone();
    let recommended_action =
        (!trust.recommended_action.trim().is_empty()).then(|| trust.recommended_action.clone());
    let recommended_action_template = recommended_action
        .as_deref()
        .and_then(action_template)
        .or_else(|| trust.recommended_action_template.clone());
    let task_assignment_start = Instant::now();
    let task_assignment_id = active_task_assignment_id(repo)?;
    let output_task_assignment_ms = task_assignment_start.elapsed().as_millis();
    let principal_start = Instant::now();
    let principal_source = verbs::resolve_principal(repo, user_config.principal_pair())?
        .source
        .unwrap_or("unknown")
        .to_string();
    let output_principal_ms = principal_start.elapsed().as_millis();
    let warnings = bulk_capture_warning(report.captured_path_count)
        .into_iter()
        .collect();
    let output = SnapshotOutput {
        output_kind: "capture",
        status: "captured",
        action: "capture",
        state_id: report.state_id.short(),
        content_hash: report.content_hash.short(),
        intent: report.intent,
        confidence: report.confidence,
        task_assignment_id,
        principal: (&report.principal).into(),
        principal_source,
        agent: report.agent.as_ref().map(SnapshotAgentOutput::from),
        promotion_suggested: report.promotion_suggested,
        heavy_impact_paths: report.heavy_impact_paths.clone(),
        captured_path_count: report.captured_path_count,
        warnings,
        signed: report.signed,
        message: report.summary,
        next_action: recommended_action.clone(),
        next_action_template: recommended_action_template.clone(),
        recommended_action,
        recommended_action_template,
        trust,
    };
    let mut profile = snapshot_command_profile(
        report.snapshot_profile,
        report.state_create_ms,
        report.captured_path_count_ms,
        report.post_verification_ms,
        report.thread_metadata_ms,
    );
    profile.output_build_ms = output_build_start.elapsed().as_millis();
    profile.output_task_assignment_ms = output_task_assignment_ms;
    profile.output_principal_ms = output_principal_ms;
    profile.previous_state_ms = previous_state_ms;
    profile.previous_state_head_ms = previous_state_profile.head_ms;
    profile.previous_state_cache_read_ms = previous_state_profile.cache_read_ms;
    profile.previous_state_cache_decode_ms = previous_state_profile.cache_decode_ms;
    profile.previous_state_cache_validate_ms = previous_state_profile.cache_validate_ms;
    profile.previous_state_store_read_ms = previous_state_profile.store_read_ms;
    profile.previous_state_cache_hit = previous_state_profile.cache_hit;
    profile.signature_lookup_ms = signature_lookup_ms;
    Ok((output, profile))
}

const BULK_CAPTURE_WARNING_THRESHOLD: usize = 500;

fn bulk_capture_warning(captured_path_count: usize) -> Option<String> {
    (captured_path_count >= BULK_CAPTURE_WARNING_THRESHOLD).then(|| {
        format!(
            "captured {captured_path_count} paths in one operation; check root .gitignore and .heddleignore rules if build artifacts or tool state were included"
        )
    })
}

fn active_task_assignment_id(repo: &Repository) -> Result<Option<String>> {
    let Some(thread) = current_thread(repo)? else {
        return Ok(None);
    };
    Ok(find_active_thread_entry(repo, &thread.id)?.and_then(|entry| entry.task_assignment_id))
}

fn default_bootstrap_intent(repo: &Repository) -> String {
    match repo.head_ref() {
        Ok(refs::Head::Attached { thread }) => format!("Bootstrap git-overlay on {}", thread),
        _ => "Bootstrap git-overlay state".to_string(),
    }
}

fn snapshot_command_profile(
    repo_profile: SnapshotProfile,
    state_create_ms: u128,
    captured_path_count_ms: u128,
    post_verification_ms: u128,
    thread_metadata_ms: u128,
) -> SnapshotCommandProfile {
    SnapshotCommandProfile {
        tree_walk_ms: repo_profile.tree_walk_ms,
        blob_prep_ms: repo_profile.blob_prep_ms,
        blob_write_ms: repo_profile.blob_write_ms,
        tree_write_ms: repo_profile.tree_write_ms,
        state_ref_oplog_ms: repo_profile.state_ref_oplog_ms,
        atomic_execute_ms: repo_profile.atomic_execute_ms,
        ref_publish_ms: repo_profile.ref_publish_ms,
        state_create_ms,
        captured_path_count_ms,
        post_verification_ms,
        thread_metadata_ms,
        output_build_ms: 0,
        output_task_assignment_ms: 0,
        output_principal_ms: 0,
        preflight_ms: 0,
        attribution_ms: 0,
        execute_save_ms: 0,
        previous_state_ms: 0,
        previous_state_head_ms: 0,
        previous_state_cache_read_ms: 0,
        previous_state_cache_decode_ms: 0,
        previous_state_cache_validate_ms: 0,
        previous_state_store_read_ms: 0,
        previous_state_cache_hit: false,
        signature_lookup_ms: 0,
    }
}

pub(crate) fn build_attribution(
    repo: &Repository,
    user_config: &UserConfig,
    agent: &SnapshotAgentOverrides,
) -> Result<Attribution> {
    build_capture_attribution(repo, user_config, agent).map(|resolved| resolved.attribution)
}

fn build_capture_attribution(
    repo: &Repository,
    user_config: &UserConfig,
    agent: &SnapshotAgentOverrides,
) -> Result<verbs::CaptureAttribution> {
    let resolved_principal = verbs::resolve_principal(repo, user_config.principal_pair())?;
    let principal_source = resolved_principal.source.unwrap_or("unknown").to_string();
    let principal = resolved_principal.principal;
    if is_default_unknown_principal(&principal) {
        return Err(anyhow!(missing_capture_identity_advice()));
    }

    if agent.no_agent {
        return Ok(verbs::CaptureAttribution {
            attribution: Attribution::human(principal),
            principal_source,
            harness_session_id: None,
        });
    }

    // Put state in. Do not hunt `/proc`, hoped-for env, thread actor, or
    // repo/user `agent.model` after a cursor miss. Cursor/Grok stay
    // `agent=null`. Never invent a model.
    let frozen = crate::identity_freeze::freeze_identity_for_capture(repo).unwrap_or_default();
    let harness_session_id = frozen.session.clone();
    let current_session = SessionManager::new(repo.root()).get_current_session()?;
    let provider = agent
        .provider
        .clone()
        .or(frozen.provider.clone())
        .and_then(clean_attribution_value);
    let model = agent
        .model
        .clone()
        .or(frozen.model.clone())
        .and_then(clean_attribution_value);
    let session_policy = current_session
        .as_ref()
        .and_then(|session| session.current_segment())
        .and_then(|segment| segment.policy_id.clone())
        .and_then(clean_attribution_value);
    // Heddle Session.id — never the sidecar harness session.
    let session_id = agent
        .session
        .clone()
        .or_else(|| current_session.as_ref().map(|session| session.id.clone()));
    let segment_id = agent
        .segment
        .clone()
        .or(frozen.segment_id.clone())
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
            .or_else(|| {
                std::env::var("HEDDLE_AGENT_POLICY")
                    .ok()
                    .and_then(clean_attribution_value)
            })
            .or(session_policy)
            .or_else(|| user_config.agent.default_policy.clone())
            .or_else(|| repo.config().policies.default_policy.clone())
    };

    let attribution = match (provider, model) {
        (Some(p), Some(m)) => {
            let mut agent = Agent::new(p, m);
            if let (Some(sid), Some(segid)) = (session_id, segment_id) {
                agent = agent.with_session(sid, segid);
            }
            if let Some(pol) = policy {
                agent = agent.with_policy(pol);
            }
            if let Some(thought_level) = frozen.thought_level {
                agent = agent.with_thought_level(thought_level);
            }
            if let Some(parent) = frozen.parent {
                agent = agent.with_parent(parent);
            }
            Attribution::with_agent(principal, agent)
        }
        _ => Attribution::human(principal),
    };
    Ok(verbs::CaptureAttribution {
        attribution,
        principal_source,
        harness_session_id,
    })
}

// Attribution resolution lives in `hosted-client` so the hosted context sync
// and the CLI verbs share one implementation.
pub(crate) use hosted_client::attribution::{resolve_attribution, resolve_principal};

pub(crate) fn is_placeholder_principal(principal: &Principal) -> bool {
    let name = principal.name_lossy();
    let email = principal.email_lossy();
    let name = name.trim();
    let email = email.trim().to_ascii_lowercase();
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
    principal_lacks_accountable_identity(&principal.name_lossy(), &principal.email_lossy())
}

#[cfg(test)]
mod tests {
    use std::{
        process::Command,
        sync::{Arc, Barrier},
        thread,
    };

    use super::*;

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }

        fn remove(key: &'static str) -> Self {
            let previous = std::env::var(key).ok();
            unsafe { std::env::remove_var(key) };
            Self { key, previous }
        }
    }

    fn isolate_child_identity_env() -> Vec<EnvVarGuard> {
        [
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_EFFORT",
            "PI_MODEL",
            "PI_REASONING_LEVEL",
            "PI_SESSION_ID",
            "PI_PROVIDER",
            "PI_PARENT_ID",
        ]
        .into_iter()
        .map(EnvVarGuard::remove)
        .collect()
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

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
    ) -> repo::ActorPresence {
        let thread = current_thread(repo)
            .unwrap()
            .expect("initialized repository has a current thread");
        let registry = repo::ActorPresenceStore::new(repo.heddle_dir());
        let entry = repo::ActorPresence {
            session_id: repo::generate_actor_session_id(),
            client_instance_id: None,
            native_actor_key: Some("claude-code:session:session-457".to_string()),
            native_parent_actor_key: None,
            native_instance_key: Some("claude-code:transcript:/tmp/claude/457.jsonl".to_string()),
            heddle_session_id: None,
            thread_id: Some(thread.id.clone()),
            thread: thread.id,
            anchor_state: None,
            anchor_root: None,
            path: Some(repo.root().to_path_buf()),
            base_state: String::new(),
            started_at: chrono::Utc::now(),
            provider: Some(provider.to_string()),
            model: Some(model.to_string()),
            harness: Some("claude-code".to_string()),
            thinking_level: None,
            usage_summary: repo::AgentUsageSummary::default(),
            last_progress_at: None,
            report_flush_state: Some("pending-local".to_string()),
            attach_reason: Some("test detected harness actor".to_string()),
            task_assignment_id: None,
            attach_precedence: Vec::new(),
            winning_attach_rule: Some("test".to_string()),
            probe_source: Some("hook_payload".to_string()),
            probe_confidence: Some(0.99),
            status: repo::ActorPresenceStatus::Active,
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
    #[serial_test::serial]
    fn cursor_grok_stays_agent_null_when_env_or_repo_model_set() {
        let _child = isolate_child_identity_env();
        let _model = EnvVarGuard::set("HEDDLE_AGENT_MODEL", "claude-opus-4-7");
        let _provider = EnvVarGuard::set("HEDDLE_AGENT_PROVIDER", "anthropic");
        let temp = tempfile::TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        save_active_harness_entry(&repo, "anthropic", "claude-opus-4-8[1m]");
        let config_path = temp.path().join(".heddle/config.toml");
        let mut config = repo::RepoConfig::load(&config_path).unwrap();
        config.agent.provider = Some("anthropic".into());
        config.agent.model = Some("claude-opus-4-7".into());
        config.save(&config_path).unwrap();
        let repo = Repository::open(temp.path()).unwrap();

        let mut user_config = user_config_with_principal();
        user_config.agent.provider = Some("openai".into());
        user_config.agent.model = Some("gpt-5".into());

        let attribution = build_attribution(&repo, &user_config, &empty_agent_overrides()).unwrap();
        assert!(
            attribution.agent.is_none(),
            "Cursor/Grok must stay agent=null; do not invent from env, repo, or thread actor"
        );
    }

    #[test]
    #[serial_test::serial]
    fn model_change_mid_thread_rotates_segment_and_leaves_old_state() {
        let _child = isolate_child_identity_env();
        let temp = tempfile::TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let mut manager = SessionManager::new(repo.root());
        manager
            .start_session(
                objects::object::Principal::new("Ada Lovelace", "ada@example.com"),
                "anthropic".into(),
                "opus".into(),
                None,
            )
            .unwrap();
        verbs::write_identity_cursor(
            repo.root(),
            &verbs::IdentityCursor {
                provider: Some("anthropic".into()),
                model: Some("opus".into()),
                thought_level: Some("high".into()),
                session: Some("sess-live".into()),
                parent: Some("agent-1".into()),
            },
        )
        .unwrap();
        let first = build_attribution(
            &repo,
            &user_config_with_principal(),
            &empty_agent_overrides(),
        )
        .unwrap();
        let first_agent = first.agent.expect("cursor should freeze agent");
        assert_eq!(first_agent.model, "opus");
        assert_eq!(first_agent.thought_level.as_deref(), Some("high"));
        assert_eq!(first_agent.parent.as_deref(), Some("agent-1"));
        let first_segment = first_agent.segment_id.clone();

        verbs::write_identity_cursor(
            repo.root(),
            &verbs::IdentityCursor {
                provider: Some("anthropic".into()),
                model: Some("sonnet".into()),
                thought_level: Some("low".into()),
                session: Some("sess-live".into()),
                parent: Some("agent-1".into()),
            },
        )
        .unwrap();
        let second = build_attribution(
            &repo,
            &user_config_with_principal(),
            &empty_agent_overrides(),
        )
        .unwrap();
        let second_agent = second.agent.expect("updated cursor should freeze");
        assert_eq!(second_agent.model, "sonnet");
        assert_eq!(second_agent.thought_level.as_deref(), Some("low"));
        assert_ne!(second_agent.segment_id, first_segment);
        assert_eq!(first_agent.model, "opus");
        assert_eq!(first_agent.thought_level.as_deref(), Some("high"));
        assert_eq!(first_agent.segment_id, first_segment);
    }

    #[test]
    #[serial_test::serial]
    fn session_end_expire_leaves_human_capture() {
        let _child = isolate_child_identity_env();
        let temp = tempfile::TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        verbs::write_identity_cursor(
            repo.root(),
            &verbs::IdentityCursor {
                provider: Some("anthropic".into()),
                model: Some("opus".into()),
                thought_level: Some("high".into()),
                session: Some("dead-session".into()),
                parent: Some("agent-1".into()),
            },
        )
        .unwrap();
        verbs::expire_identity_cursor(repo.root()).unwrap();
        let attribution = build_attribution(
            &repo,
            &user_config_with_principal(),
            &empty_agent_overrides(),
        )
        .unwrap();
        assert!(
            attribution.agent.is_none(),
            "expired SessionEnd cursor must not invent an agent on the next capture"
        );
    }

    #[test]
    #[serial_test::serial]
    fn codex_stop_expire_leaves_human_capture() {
        let _child = isolate_child_identity_env();
        let temp = tempfile::TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        crate::identity_stamp::stamp_bytes(
            repo.root(),
            "codex",
            r#"{"model":"gpt-5.4","session_id":"c1"}"#,
        )
        .unwrap();
        assert_eq!(
            verbs::read_identity_cursor(repo.root()).model.as_deref(),
            Some("gpt-5.4")
        );
        crate::identity_stamp::stamp_bytes(
            repo.root(),
            "codex",
            r#"{"hook_event_name":"Stop","session_id":"c1"}"#,
        )
        .unwrap();
        let attribution = build_attribution(
            &repo,
            &user_config_with_principal(),
            &empty_agent_overrides(),
        )
        .unwrap();
        assert!(
            attribution.agent.is_none(),
            "expired Codex Stop cursor must not invent an agent on the next capture"
        );
    }

    #[test]
    #[serial_test::serial]
    fn build_attribution_omits_unpublished_cursor_fields() {
        let _child = isolate_child_identity_env();
        let temp = tempfile::TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        verbs::write_identity_cursor(
            repo.root(),
            &verbs::IdentityCursor {
                provider: Some("anthropic".into()),
                model: Some("opus".into()),
                ..verbs::IdentityCursor::default()
            },
        )
        .unwrap();
        let attribution = build_attribution(
            &repo,
            &user_config_with_principal(),
            &empty_agent_overrides(),
        )
        .unwrap();
        let agent = attribution.agent.expect("published model should freeze");
        assert!(agent.thought_level.is_none());
        assert!(agent.parent.is_none());
        assert!(
            agent.session_id.is_none()
                || !agent
                    .session_id
                    .as_deref()
                    .is_some_and(|s| s == "sess-live"),
            "harness session must not be stuffed into Heddle Session.id"
        );
    }

    #[test]
    fn concurrent_overlay_bootstraps_bind_latest_locked_git_tip_once() {
        let temp = tempfile::TempDir::new().expect("create Git fixture");
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(temp.path())
                .output()
                .expect("run git");
            assert!(
                output.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout)
                .expect("Git output is UTF-8")
                .trim()
                .to_string()
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.name", "Overlay Test"]);
        git(&["config", "user.email", "overlay@example.com"]);
        std::fs::write(temp.path().join("README.md"), "old\n").expect("write old tip");
        git(&["add", "README.md"]);
        git(&["commit", "-m", "old tip"]);
        let old_tip = git(&["rev-parse", "HEAD"]);

        let repo = Repository::bootstrap_git_overlay(temp.path()).expect("bootstrap overlay");
        let authoritative_update = repo.locker().write().expect("hold repository lock");
        let ready = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let root = temp.path().to_path_buf();
            let ready = Arc::clone(&ready);
            workers.push(thread::spawn(move || {
                let repo = Repository::open(root).expect("open worker repository");
                ready.wait();
                bind_git_overlay_active_tip(&repo).expect("bind authoritative Git tip")
            }));
        }
        ready.wait();

        // Both simulated commands are now contending on the repository lock.
        // Advance authoritative Git while holding it; neither command may have
        // resolved the stale tip before the publication transaction begins.
        std::fs::write(temp.path().join("README.md"), "new\n").expect("write new tip");
        git(&["add", "README.md"]);
        git(&["commit", "-m", "new tip"]);
        let new_tip = git(&["rev-parse", "HEAD"]);
        drop(authoritative_update);

        let states = workers
            .into_iter()
            .map(|worker| {
                worker
                    .join()
                    .expect("join bootstrap worker")
                    .expect("state")
            })
            .collect::<Vec<_>>();
        assert_eq!(states[0], states[1]);
        let repo = Repository::open(temp.path()).expect("reopen repository");
        assert_eq!(
            repo.current_state()
                .expect("read current state")
                .expect("bound state")
                .state_id,
            states[0]
        );
        assert!(
            repo.git_overlay_mapped_state_for_git_commit(&old_tip)
                .expect("read old mapping")
                .is_none(),
            "no contender may publish the Git tip observed before lock acquisition"
        );
        let checkpoints = repo.list_git_checkpoints().expect("read checkpoints");
        assert_eq!(
            checkpoints.len(),
            1,
            "checkpoint RMW must not lose or duplicate"
        );
        assert_eq!(checkpoints[0].git_commit, new_tip);
        assert_eq!(checkpoints[0].state_id, states[0].to_string_full());
    }

    #[test]
    fn detached_overlay_bootstrap_detaches_heddle_without_moving_default_thread() {
        let temp = tempfile::TempDir::new().expect("create Git fixture");
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(temp.path())
                .output()
                .expect("run git");
            assert!(
                output.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout)
                .expect("Git output is UTF-8")
                .trim()
                .to_string()
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.name", "Overlay Test"]);
        git(&["config", "user.email", "overlay@example.com"]);
        std::fs::write(temp.path().join("README.md"), "detached\n").expect("write tip");
        git(&["add", "README.md"]);
        git(&["commit", "-m", "detached tip"]);
        let tip = git(&["rev-parse", "HEAD"]);
        git(&["checkout", "--detach", &tip]);

        let repo = Repository::bootstrap_git_overlay(temp.path()).expect("bootstrap overlay");
        let state = bind_git_overlay_active_tip(&repo)
            .expect("bind detached tip")
            .expect("mapped state");

        assert_eq!(
            repo.refs().read_head().expect("read raw Heddle HEAD"),
            Head::Detached { state }
        );
        assert!(
            repo.refs()
                .get_thread(&ThreadName::new("main"))
                .expect("read default thread")
                .is_none(),
            "detached Git bootstrap must not publish through the sidecar's default attached thread"
        );
    }

    #[test]
    fn bulk_capture_warning_starts_at_five_hundred_paths() {
        assert!(bulk_capture_warning(BULK_CAPTURE_WARNING_THRESHOLD - 1).is_none());
        let warning = bulk_capture_warning(BULK_CAPTURE_WARNING_THRESHOLD)
            .expect("the threshold should emit a warning");
        assert!(warning.contains("captured 500 paths"));
        assert!(warning.contains(".gitignore"));
        assert!(warning.contains(".heddleignore"));
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
