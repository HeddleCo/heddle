// SPDX-License-Identifier: Apache-2.0
use std::{collections::BTreeSet, path::Path};

use anyhow::Result;
use chrono::Utc;
use heddle_core::{
    VerificationClaimPolicyFacts,
    raw_git_preservation_command as core_raw_git_preservation_command,
    repository_verification_allows_success_claim as core_repository_verification_allows_success_claim,
    status::next_action::{NextActionInput, effective_next_action, non_empty_action},
};
use objects::{object::ThreadName, store::ObjectStore};
use repo::{
    GitImportGuidance, GitRemoteTrackingStatus, OperationKind, OperationScope, Repository,
    RepositoryOperationStatus, ThreadFreshness, ThreadIntegrationPolicy, ThreadManager,
    ThreadState, shell_quote, update_thread_state_from_state,
};
use serde::{Serialize, Serializer, ser::SerializeStruct};
use sley::{IndexStage, Repository as SleyRepository};

use super::{
    rebase::{
        OperatorContinueStatus, cmd_rebase_silent, continue_rebase_for_operator,
        has_persisted_rebase_state,
    },
    resolve::abort_merge_state,
    snapshot::{SnapshotAgentOverrides, create_snapshot},
    verification_health::{
        RepositoryVerificationState, action_template, repository_verification_blockers,
        repository_verification_primary_command,
    },
};
use crate::config::UserConfig;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OperatorAction {
    Abort,
    Bisect,
    CherryPick,
    #[default]
    Continue,
    Land,
    Merge,
    Ready,
    Rebase,
    Revert,
    Sync,
    ThreadCleanup,
    ThreadDrop,
    ThreadPromote,
    ThreadRefresh,
    ThreadResolve,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OperatorEmission {
    pub(crate) command: &'static [&'static str],
    pub(crate) output_kind: OperatorAction,
}

pub(crate) const ABORT_OPERATOR_EMISSION: OperatorEmission = OperatorEmission {
    command: &["abort"],
    output_kind: OperatorAction::Abort,
};

pub(crate) const CONTINUE_OPERATOR_EMISSION: OperatorEmission = OperatorEmission {
    command: &["continue"],
    output_kind: OperatorAction::Continue,
};

pub(crate) const SYNC_OPERATOR_EMISSION: OperatorEmission = OperatorEmission {
    command: &["sync"],
    output_kind: OperatorAction::Sync,
};

pub(crate) const OPERATOR_EMISSIONS: &[OperatorEmission] = &[
    ABORT_OPERATOR_EMISSION,
    CONTINUE_OPERATOR_EMISSION,
    SYNC_OPERATOR_EMISSION,
];

pub fn operator_emission_output_kinds() -> Vec<(String, String)> {
    OPERATOR_EMISSIONS
        .iter()
        .map(|emission| {
            (
                emission.command.join(" "),
                emission.output_kind.wire_value().to_string(),
            )
        })
        .collect()
}

impl OperatorAction {
    const fn wire_value(self) -> &'static str {
        match self {
            Self::Abort => "abort",
            Self::Bisect => "bisect",
            Self::CherryPick => "cherry-pick",
            Self::Continue => "continue",
            Self::Land => "land",
            Self::Merge => "merge",
            Self::Ready => "ready",
            Self::Rebase => "rebase",
            Self::Revert => "revert",
            Self::Sync => "sync",
            Self::ThreadCleanup => "thread_cleanup",
            Self::ThreadDrop => "thread_drop",
            Self::ThreadPromote => "thread_promote",
            Self::ThreadRefresh => "thread_refresh",
            Self::ThreadResolve => "thread_resolve",
        }
    }
}

impl Serialize for OperatorAction {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.wire_value())
    }
}

impl From<&OperationKind> for OperatorAction {
    fn from(kind: &OperationKind) -> Self {
        match kind {
            OperationKind::Merge => Self::Merge,
            OperationKind::Rebase => Self::Rebase,
            OperationKind::CherryPick => Self::CherryPick,
            OperationKind::Revert => Self::Revert,
            OperationKind::Bisect => Self::Bisect,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct OperatorCommandOutput {
    pub status: String,
    pub action: OperatorAction,
    pub message: String,
    /// Reasons the operation could not advance state. Only populated
    /// when `status == "blocked"` or `status == "failed"`. When the
    /// operation succeeded with caveats, use `warnings` instead.
    pub blockers: Vec<String>,
    /// Non-blocking nudges surfaced when the operation actually
    /// advanced state but the caller may still want a follow-up
    /// (e.g. a heavy-impact change worth reviewing for broader impact).
    /// Always omitted when empty.
    pub warnings: Vec<String>,
    pub next_action: Option<String>,
    pub recommended_action: Option<String>,
}

impl OperatorCommandOutput {
    pub(crate) fn blocked_by_repository_verification(
        action: OperatorAction,
        message: impl Into<String>,
        trust: &RepositoryVerificationState,
    ) -> Self {
        let recommended_action = repository_verification_primary_command(trust);
        Self {
            status: "blocked".to_string(),
            action,
            message: message.into(),
            blockers: repository_verification_blockers(trust),
            warnings: Vec::new(),
            next_action: Some(recommended_action.clone()),
            recommended_action: Some(recommended_action),
        }
    }

    pub(crate) fn block_success_claim_if_verification_blocked(
        &mut self,
        trust: &RepositoryVerificationState,
        local_context: impl Into<String>,
        policy: VerificationClaimPolicy,
    ) {
        if repository_verification_allows_success_claim(self, trust, policy) {
            return;
        }
        *self = Self::blocked_by_repository_verification(
            self.action,
            format!(
                "{} reached local checks, but repository verification is blocked: {}",
                local_context.into(),
                trust.summary
            ),
            trust,
        );
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct VerificationClaimPolicy {
    allow_land_publish_followup: bool,
    allow_matching_workflow_action: bool,
}

impl VerificationClaimPolicy {
    pub(crate) fn strict() -> Self {
        Self::default()
    }

    pub(crate) fn allow_land_publish_followup(mut self) -> Self {
        self.allow_land_publish_followup = true;
        self
    }

    pub(crate) fn allow_matching_workflow_action(mut self) -> Self {
        self.allow_matching_workflow_action = true;
        self
    }
}

fn repository_verification_allows_success_claim(
    output: &OperatorCommandOutput,
    trust: &RepositoryVerificationState,
    policy: VerificationClaimPolicy,
) -> bool {
    use heddle_core::VerificationClaimTrustFacts;
    core_repository_verification_allows_success_claim(
        &output.status,
        VerificationClaimTrustFacts {
            verified: trust.verified,
            recommended_action: &trust.recommended_action,
            remote_drift: &trust.remote_drift,
            workflow_status: &trust.workflow_status,
        },
        output.action == OperatorAction::Land && output.status == "landed",
        output
            .recommended_action
            .as_deref()
            .is_some_and(|action| action == trust.recommended_action),
        VerificationClaimPolicyFacts {
            allow_land_publish_followup: policy.allow_land_publish_followup,
            allow_matching_workflow_action: policy.allow_matching_workflow_action,
        },
    )
}

/// True when an operator envelope's `status` is a non-success terminal
/// outcome that scripts must observe as a non-zero process exit.
pub(crate) fn is_blocked_operator_status(status: &str) -> bool {
    matches!(status, "blocked" | "failed")
}

/// After the command has rendered its operator envelope, convert a blocked
/// or failed status into a typed error so `main` can map it through
/// [`crate::exit::HeddleExitCode::from_error`] without a second envelope
/// (see [`crate::exit::OutcomeExit`]).
pub(crate) fn fail_if_blocked_operator_status(status: &str) -> Result<()> {
    if is_blocked_operator_status(status) {
        return Err(anyhow::anyhow!(crate::exit::OutcomeExit::data_err()));
    }
    Ok(())
}

impl Serialize for OperatorCommandOutput {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.serialize_with_output_kind(serializer, self.action)
    }
}

impl OperatorCommandOutput {
    pub(crate) fn envelope_for_command(
        &self,
        output_kind: OperatorAction,
    ) -> OperatorCommandEnvelope<'_> {
        OperatorCommandEnvelope {
            output: self,
            output_kind,
        }
    }

    fn serialize_with_output_kind<S>(
        &self,
        serializer: S,
        output_kind: OperatorAction,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let next_action = normalized_action(self.next_action.as_deref());
        let recommended_action = normalized_action(self.recommended_action.as_deref());
        let next_action_template = next_action.and_then(action_template);
        let recommended_action_template = recommended_action.and_then(action_template);

        let mut len = 8;
        if !self.blockers.is_empty() {
            len += 1;
        }
        if !self.warnings.is_empty() {
            len += 1;
        }

        let mut state = serializer.serialize_struct("OperatorCommandOutput", len)?;
        state.serialize_field("output_kind", &output_kind.wire_value())?;
        state.serialize_field("status", &self.status)?;
        state.serialize_field("action", &self.action)?;
        state.serialize_field("message", &self.message)?;
        if !self.blockers.is_empty() {
            state.serialize_field("blockers", &self.blockers)?;
        }
        if !self.warnings.is_empty() {
            state.serialize_field("warnings", &self.warnings)?;
        }
        state.serialize_field("next_action", &next_action)?;
        state.serialize_field("next_action_template", &next_action_template)?;
        state.serialize_field("recommended_action", &recommended_action)?;
        state.serialize_field("recommended_action_template", &recommended_action_template)?;
        state.end()
    }
}

pub(crate) struct OperatorCommandEnvelope<'a> {
    output: &'a OperatorCommandOutput,
    output_kind: OperatorAction,
}

impl Serialize for OperatorCommandEnvelope<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.output
            .serialize_with_output_kind(serializer, self.output_kind)
    }
}

fn normalized_action(action: Option<&str>) -> Option<&str> {
    non_empty_action(action)
}

impl super::compact::CompactProjection for OperatorCommandOutput {
    /// The shared compact core for the whole operator family. `merge`,
    /// `ready`, `continue`, `abort`, `sync`, and `land` all build their
    /// compact projection from this so the decision surface stays in
    /// lockstep with the embedded `OperatorCommandOutput`. Commands that
    /// also carry changed-path / conflict axes layer those on top of the
    /// returned value.
    fn compact(&self) -> super::compact::CompactOutput {
        self.compact_with_output_kind(self.action)
    }
}

impl OperatorCommandOutput {
    fn compact_with_output_kind(
        &self,
        output_kind: OperatorAction,
    ) -> super::compact::CompactOutput {
        // Prefer the validated `recommended_action`; fall back to
        // `next_action`. Both are the same canonical breadcrumb in the
        // full envelope — compact emits exactly one, as `next_action`.
        let action = normalized_action(self.recommended_action.as_deref())
            .or_else(|| normalized_action(self.next_action.as_deref()));
        let mut compact = super::compact::CompactOutput::new(output_kind.wire_value());
        compact.status = Some(self.status.clone());
        compact.blockers = self.blockers.clone();
        compact.next_action = action.map(str::to_string);
        compact.next_action_template = action.and_then(action_template);
        compact
    }
}

impl super::compact::CompactProjection for OperatorCommandEnvelope<'_> {
    fn compact(&self) -> super::compact::CompactOutput {
        self.output.compact_with_output_kind(self.output_kind)
    }
}

pub(crate) fn open_operator_repo_from_path(path: &Path) -> Result<Repository> {
    let cwd_repo = Repository::open(path)?;
    let target_path = cwd_repo.active_worktree_path()?;
    if target_path == *cwd_repo.root() {
        Ok(cwd_repo)
    } else {
        Ok(Repository::open(&target_path)?)
    }
}

pub(crate) fn continue_operator(repo: &Repository) -> Result<OperatorCommandOutput> {
    if repo.merge_state_manager().is_merge_in_progress() {
        let unresolved = repo.merge_state_manager().unresolved()?;
        if !unresolved.is_empty() {
            // A conflict path can legitimately contain spaces, so shell-quote
            // it: this is a *validated* recommended_action (write_validated_json_stdout
            // tokenizes it), and an unquoted space would split into extra args
            // and fail the next_action validator. (heddle#464 close-the-class.)
            let recommended_action = format!("heddle resolve {}", shell_quote(&unresolved[0]));
            return Ok(OperatorCommandOutput {
                status: "blocked".to_string(),
                action: OperatorAction::Merge,
                message: format!(
                    "Merge still has unresolved conflicts: {}. After removing conflict markers, mark each file resolved with `heddle resolve <path>`.",
                    unresolved.join(", ")
                ),
                blockers: unresolved,
                warnings: Vec::new(),
                next_action: Some("heddle resolve --list".to_string()),
                recommended_action: Some(recommended_action),
            });
        }

        create_snapshot(
            repo,
            &UserConfig::load_default()?,
            Some("Continue merge".to_string()),
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
        let next_action = complete_current_thread_manual_resolution(repo)?;
        return Ok(OperatorCommandOutput {
            status: "continued".to_string(),
            action: OperatorAction::Merge,
            message: "Completed the in-progress Heddle merge".to_string(),
            blockers: Vec::new(),
            warnings: Vec::new(),
            next_action: next_action.clone(),
            recommended_action: next_action,
        });
    }

    if let Some(operation) = repo.operation_status()? {
        return continue_from_operation(repo, &operation);
    }

    Ok(OperatorCommandOutput {
        status: "noop".to_string(),
        action: OperatorAction::Continue,
        message: "No in-progress operation needs continuing".to_string(),
        blockers: Vec::new(),
        warnings: Vec::new(),
        next_action: None,
        recommended_action: None,
    })
}

pub(crate) fn abort_operator(repo: &Repository) -> Result<OperatorCommandOutput> {
    if repo.merge_state_manager().is_merge_in_progress() {
        abort_merge_state(repo, &repo.merge_state_manager())?;
        return Ok(OperatorCommandOutput {
            status: "aborted".to_string(),
            action: OperatorAction::Merge,
            message: "Aborted the in-progress Heddle merge".to_string(),
            blockers: Vec::new(),
            warnings: Vec::new(),
            next_action: None,
            recommended_action: None,
        });
    }

    if has_persisted_rebase_state(repo) {
        cmd_rebase_silent(repo, None, true, false)?;
        return Ok(OperatorCommandOutput {
            status: "aborted".to_string(),
            action: OperatorAction::Rebase,
            message: "Aborted the in-progress Heddle rebase".to_string(),
            blockers: Vec::new(),
            warnings: Vec::new(),
            next_action: None,
            recommended_action: None,
        });
    }

    if let Some(operation) = repo.operation_status()? {
        return abort_from_operation(repo, &operation);
    }

    Ok(OperatorCommandOutput {
        status: "noop".to_string(),
        action: OperatorAction::Abort,
        message: "No in-progress operation can be aborted".to_string(),
        blockers: Vec::new(),
        warnings: Vec::new(),
        next_action: None,
        recommended_action: None,
    })
}

pub(crate) fn complete_current_thread_manual_resolution(
    repo: &Repository,
) -> Result<Option<String>> {
    let Some(current_thread) = repo.current_lane()? else {
        return Ok(None);
    };
    let Some(current_state) = repo.head()? else {
        return Ok(None);
    };
    let Some(current_state_obj) = repo.store().get_state(&current_state)? else {
        return Ok(None);
    };

    let manager = ThreadManager::new(repo.heddle_dir());
    let Some(mut thread) = manager.find_by_thread(&current_thread)? else {
        return Ok(None);
    };
    let Some(target_thread) = thread.target_thread.clone() else {
        return Ok(None);
    };
    let Some(target_state) = repo.refs().get_thread(&ThreadName::new(&target_thread))? else {
        return Ok(None);
    };
    let Some(target_state_obj) = repo.store().get_state(&target_state)? else {
        return Ok(None);
    };
    let before_update = super::thread_cmd::capture_thread_update_before(repo, &manager, &thread)?;

    thread.base_state = target_state.short();
    thread.base_root = target_state_obj.tree.short();
    update_thread_state_from_state(&mut thread, &current_state_obj);
    thread.state = ThreadState::Ready;
    thread.freshness = ThreadFreshness::Current;
    thread.integration_policy_result = ThreadIntegrationPolicy {
        status: Some("manual_resolved".to_string()),
        reason: Some("manual conflict resolution captured".to_string()),
        manual_resolution_state: Some(current_state.short()),
        conflicts_resolved_manually: true,
    };
    thread.updated_at = Utc::now();
    let thread_id = thread.id.clone();
    let target = thread.target_thread.clone();
    super::thread_cmd::save_thread_update_with_oplog(
        repo,
        &manager,
        &thread,
        before_update,
        current_state,
    )?;

    let action = super::thread_landing::land_command_for_thread(repo, &thread_id);
    Ok(Some(super::thread::contextual_thread_action(
        repo,
        &thread_id,
        target.as_deref(),
        &action,
    )))
}

fn continue_from_operation(
    repo: &Repository,
    operation: &RepositoryOperationStatus,
) -> Result<OperatorCommandOutput> {
    match (&operation.scope, &operation.kind) {
        (OperationScope::Heddle, OperationKind::Rebase) => {
            Ok(match continue_rebase_for_operator(repo)? {
                OperatorContinueStatus::Blocked => OperatorCommandOutput {
                    status: "blocked".to_string(),
                    action: OperatorAction::Rebase,
                    message:
                        "Rebase still needs a captured manual resolution before it can continue"
                            .to_string(),
                    blockers: Vec::new(),
                    warnings: Vec::new(),
                    next_action: Some("heddle commit -m \"Manual resolution\"".to_string()),
                    recommended_action: Some("heddle commit -m \"Manual resolution\"".to_string()),
                },
                OperatorContinueStatus::Continued => OperatorCommandOutput {
                    status: "continued".to_string(),
                    action: OperatorAction::Rebase,
                    message: "Continued the in-progress Heddle rebase".to_string(),
                    blockers: Vec::new(),
                    warnings: Vec::new(),
                    next_action: None,
                    recommended_action: None,
                },
                OperatorContinueStatus::Completed => OperatorCommandOutput {
                    status: "completed".to_string(),
                    action: OperatorAction::Rebase,
                    message: "Completed the in-progress Heddle rebase".to_string(),
                    blockers: Vec::new(),
                    warnings: Vec::new(),
                    next_action: None,
                    recommended_action: None,
                },
            })
        }
        (OperationScope::Heddle, OperationKind::Bisect) => Ok(OperatorCommandOutput {
            status: "blocked".to_string(),
            action: OperatorAction::Bisect,
            message: "A stale bisect state from an older Heddle version is present; \
                      the bisect command has been removed. Abort to clear it."
                .to_string(),
            blockers: Vec::new(),
            warnings: Vec::new(),
            next_action: Some("heddle abort".to_string()),
            recommended_action: Some("heddle abort".to_string()),
        }),
        (OperationScope::Git, OperationKind::Rebase) => {
            let unresolved = git_unmerged_paths(repo)?;
            Ok(raw_git_operation_handoff("continue", operation, unresolved))
        }
        (OperationScope::Git, OperationKind::Merge) => {
            let unresolved = git_unmerged_paths(repo)?;
            Ok(raw_git_operation_handoff("continue", operation, unresolved))
        }
        (OperationScope::Git, OperationKind::CherryPick) => {
            let unresolved = git_unmerged_paths(repo)?;
            Ok(raw_git_operation_handoff("continue", operation, unresolved))
        }
        (OperationScope::Git, OperationKind::Revert) => {
            let unresolved = git_unmerged_paths(repo)?;
            Ok(raw_git_operation_handoff("continue", operation, unresolved))
        }
        (OperationScope::Git, OperationKind::Bisect) => {
            Ok(raw_git_operation_handoff("continue", operation, Vec::new()))
        }
        (OperationScope::Heddle, OperationKind::Merge) => unreachable!(),
        _ => Ok(OperatorCommandOutput {
            status: "noop".to_string(),
            action: OperatorAction::Continue,
            message: "No in-progress operation needs continuing".to_string(),
            blockers: Vec::new(),
            warnings: Vec::new(),
            next_action: None,
            recommended_action: None,
        }),
    }
}

fn abort_from_operation(
    repo: &Repository,
    operation: &RepositoryOperationStatus,
) -> Result<OperatorCommandOutput> {
    match (&operation.scope, &operation.kind) {
        (OperationScope::Heddle, OperationKind::Rebase) => {
            cmd_rebase_silent(repo, None, true, false)?;
        }
        (OperationScope::Heddle, OperationKind::Bisect) => {
            // Clear any stale BISECT_STATE file left by an older Heddle
            // version; the `bisect` verb itself no longer exists.
            let state_path = repo.heddle_dir().join("BISECT_STATE");
            if state_path.exists() {
                std::fs::remove_file(&state_path)?;
            }
        }
        (OperationScope::Git, _) => {
            let unresolved = git_unmerged_paths(repo).unwrap_or_default();
            return Ok(raw_git_operation_handoff("abort", operation, unresolved));
        }
        _ => {}
    }

    Ok(OperatorCommandOutput {
        status: "aborted".to_string(),
        action: OperatorAction::from(&operation.kind),
        message: format!(
            "Aborted the in-progress {} {}",
            operation.scope, operation.kind
        ),
        blockers: Vec::new(),
        warnings: Vec::new(),
        next_action: None,
        recommended_action: None,
    })
}

fn raw_git_operation_handoff(
    attempted_action: &str,
    operation: &RepositoryOperationStatus,
    unresolved: Vec<String>,
) -> OperatorCommandOutput {
    let primary = raw_git_preservation_command();
    let mut blockers = vec![format!(
        "externally-started Git {} is {}",
        operation.kind, operation.state
    )];
    blockers.extend(unresolved.iter().map(|path| format!("unresolved: {path}")));
    let unresolved_summary = if unresolved.is_empty() {
        String::new()
    } else {
        format!(" Unresolved paths: {}.", unresolved.join(", "))
    };
    let recovery_text = raw_git_operation_recovery_text(&operation.kind, &primary);
    OperatorCommandOutput {
        status: "blocked".to_string(),
        action: OperatorAction::from(&operation.kind),
        message: format!(
            "Cannot {attempted_action} the active raw Git {} inside Heddle's no-git runtime. Heddle did not start this Git sequencer operation, so it left Git metadata, refs, index, and worktree files unchanged.{unresolved_summary} {recovery_text}",
            operation.kind
        ),
        blockers,
        warnings: Vec::new(),
        next_action: Some(primary.clone()),
        recommended_action: Some(primary),
    }
}

fn raw_git_preservation_command() -> String {
    core_raw_git_preservation_command().to_string()
}

fn raw_git_operation_recovery_text(kind: &OperationKind, primary_command: &str) -> String {
    format!(
        "Inspect it with `{primary_command}`. Heddle did not start this raw Git {kind}, so finish or abort it with the Git-compatible tool that started it, then run `heddle verify` for the exact adoption command."
    )
}

pub(crate) fn recommend_next_action(
    operation: Option<&RepositoryOperationStatus>,
    remote_tracking: Option<&GitRemoteTrackingStatus>,
    import_hint: Option<&GitImportGuidance>,
    fallback: Option<&str>,
) -> String {
    effective_next_action(NextActionInput::default(
        operation,
        remote_tracking,
        import_hint,
        fallback,
    ))
}

fn git_unmerged_paths(repo: &Repository) -> Result<Vec<String>> {
    let git = match SleyRepository::discover(repo.root()) {
        Ok(git) => git,
        Err(_) => return Ok(Vec::new()),
    };
    let index = match git.open_index() {
        Ok(Some(index)) => index,
        Ok(None) => return Ok(Vec::new()),
        Err(_) => return Ok(Vec::new()),
    };
    let mut paths = BTreeSet::new();
    for entry in index.entries {
        if entry.stage() != IndexStage::Normal {
            paths.insert(String::from_utf8_lossy(entry.path.as_bytes()).into_owned());
        }
    }
    Ok(paths.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::cli::commands::verification_health::{VerificationCheck, machine_contract_coverage};

    // heddle#464 close-the-class (paths): a conflict path can contain spaces.
    // `continue` builds `recommended_action = heddle resolve <path>`, a VALIDATED
    // action. Shell-quoting the path keeps it a single token, so it survives the
    // next_action validator; leaving it bare would split into extra positionals
    // and fail validation (the render failure Codex flagged for thread ids).
    #[test]
    fn validated_resolve_action_with_spaced_path_passes_only_when_quoted() {
        use repo::shell_quote;

        use crate::cli::commands::next_action::{
            NextActionValidationContext, validated_json_string,
        };

        let path = "my conflicted file.txt";
        let context = NextActionValidationContext::without_repo(&["continue"]);

        let quoted = OperatorCommandOutput {
            status: "blocked".to_string(),
            action: OperatorAction::Continue,
            message: "conflicts remain".to_string(),
            blockers: vec![path.to_string()],
            warnings: Vec::new(),
            next_action: Some("heddle resolve --list".to_string()),
            recommended_action: Some(format!("heddle resolve {}", shell_quote(path))),
        };
        let json = validated_json_string(&quoted, context)
            .expect("a shell-quoted conflict path must pass next_action validation");
        assert!(
            json.contains("heddle resolve 'my conflicted file.txt'"),
            "the serialized recommended_action must carry the quoted path: {json}"
        );

        // Guard: the UNQUOTED interpolation is exactly the bug — it tokenizes
        // into extra positionals and fails the validator.
        let bare = OperatorCommandOutput {
            recommended_action: Some(format!("heddle resolve {path}")),
            ..quoted.clone()
        };
        assert!(
            validated_json_string(&bare, context).is_err(),
            "an unquoted spaced path must fail validation"
        );
    }

    // heddle#464 defense-in-depth — the exact P1 scenario. A blocked `land`
    // emits its `OperatorCommandOutput` (flattened into `LandOutput`) with both
    // `next_action` and `recommended_action` carrying a `heddle sync --thread
    // <id>` breadcrumb. The `<id>` is NOT guaranteed to be a freshly-validated
    // `ThreadId`: `new_unchecked` (Deserialize / `ThreadRecord::thread_id`),
    // historical records, and `heddle agent reserve --thread` all bypass
    // `validate_thread_id`. An unsafe id here (`bad;echo pwn`) would tokenize
    // into extra positionals and fail the next_action validator — the render
    // failure Codex flagged. Quoting at construction makes it a single token, so
    // the JSON validates regardless of where the id came from. This asserts the
    // P1 cannot recur.
    #[test]
    fn blocked_land_with_unvalidated_thread_id_passes_only_when_quoted() {
        use repo::shell_quote;

        use crate::cli::commands::next_action::{
            NextActionValidationContext, validated_json_string,
        };

        // Simulates a `new_unchecked` / historical / `agent reserve` id that
        // never went through `ThreadId::new`.
        let unsafe_id = "bad;echo pwn";
        let context = NextActionValidationContext::without_repo(&["land"]);

        let quoted = OperatorCommandOutput {
            status: "blocked".to_string(),
            action: OperatorAction::Land,
            message: format!("Thread '{unsafe_id}' must be synced manually"),
            blockers: vec!["thread is stale".to_string()],
            warnings: Vec::new(),
            next_action: Some(format!("heddle sync --thread {}", shell_quote(unsafe_id))),
            recommended_action: Some(format!("heddle sync --thread {}", shell_quote(unsafe_id))),
        };
        let json = validated_json_string(&quoted, context).expect(
            "a shell-quoted unvalidated thread id must pass next_action validation (the P1 fix)",
        );
        assert!(
            json.contains("heddle sync --thread 'bad;echo pwn'"),
            "both action fields must carry the quoted, single-token thread id: {json}"
        );

        // Guard: the BARE interpolation is exactly the P1 bug — the id tokenizes
        // into extra positionals (`echo`, `pwn`) and fails the validator, so the
        // JSON output would never render.
        let bare = OperatorCommandOutput {
            next_action: Some(format!("heddle sync --thread {unsafe_id}")),
            recommended_action: Some(format!("heddle sync --thread {unsafe_id}")),
            ..quoted.clone()
        };
        assert!(
            validated_json_string(&bare, context).is_err(),
            "a bare unvalidated thread id must fail validation — proving quoting is what closes the hole"
        );
    }

    #[test]
    fn raw_git_operation_handoff_recommends_heddle_preservation_not_git_cli() {
        let operation = RepositoryOperationStatus {
            scope: OperationScope::Git,
            kind: OperationKind::Merge,
            in_progress: true,
            state: "in-progress".to_string(),
            message: "Git merge is in progress".to_string(),
            next_action: raw_git_preservation_command(),
        };
        let output =
            raw_git_operation_handoff("continue", &operation, vec!["conflict.txt".to_string()]);
        assert_eq!(output.status, "blocked");
        assert_eq!(output.recommended_action.as_deref(), Some("heddle verify"));
        assert!(output.message.contains("no-git runtime"));
        assert!(output.message.contains("conflict.txt"));
        assert!(
            output
                .blockers
                .iter()
                .any(|path| path == "unresolved: conflict.txt")
        );
        assert!(
            !output
                .recommended_action
                .as_deref()
                .is_some_and(|action| action.starts_with("git "))
        );
    }

    #[test]
    fn verification_claim_gate_blocks_local_success_claims() {
        let trust = verification_state(false, "needs_checkpoint", "heddle commit -m \"...\"");
        let mut output = OperatorCommandOutput {
            status: "synced".to_string(),
            action: OperatorAction::Sync,
            message: "Thread is already current".to_string(),
            blockers: Vec::new(),
            warnings: Vec::new(),
            next_action: None,
            recommended_action: None,
        };

        output.block_success_claim_if_verification_blocked(
            &trust,
            "sync",
            VerificationClaimPolicy::strict(),
        );

        assert_eq!(output.status, "blocked");
        assert_eq!(
            output.recommended_action.as_deref(),
            Some("heddle commit -m \"...\"")
        );
        assert!(
            output
                .message
                .contains("repository verification is blocked")
        );
    }

    #[test]
    fn verification_claim_gate_allows_land_publish_followup_only_by_policy() {
        let trust = verification_state(false, "remote_ahead", "heddle push");
        let landed = || OperatorCommandOutput {
            status: "landed".to_string(),
            action: OperatorAction::Land,
            message: "Landed thread 'feature'".to_string(),
            blockers: Vec::new(),
            warnings: Vec::new(),
            next_action: Some("heddle push".to_string()),
            recommended_action: Some("heddle push".to_string()),
        };

        let mut strict = landed();
        strict.block_success_claim_if_verification_blocked(
            &trust,
            "land",
            VerificationClaimPolicy::strict(),
        );
        assert_eq!(strict.status, "blocked");

        let mut allowed = landed();
        allowed.block_success_claim_if_verification_blocked(
            &trust,
            "land",
            VerificationClaimPolicy::strict().allow_land_publish_followup(),
        );
        assert_eq!(allowed.status, "landed");
        assert_eq!(allowed.recommended_action.as_deref(), Some("heddle push"));
    }

    fn verification_state(
        verified: bool,
        status: &str,
        recommended_action: &str,
    ) -> RepositoryVerificationState {
        let check = VerificationCheck {
            name: "Worktree".to_string(),
            status: status.to_string(),
            clean: verified,
            summary: "repository verification fixture".to_string(),
            recommended_action: (!verified).then(|| recommended_action.to_string()),
            recommended_action_template: None,
            recovery_commands: if verified {
                Vec::new()
            } else {
                vec![recommended_action.to_string()]
            },
            recovery_action_templates: Vec::new(),
            details: BTreeMap::new(),
        };
        RepositoryVerificationState {
            verified,
            status: status.to_string(),
            repository_mode: "git-overlay".to_string(),
            heddle_initialized: true,
            git_branch: Some("main".to_string()),
            heddle_thread: Some("main".to_string()),
            worktree_dirty: false,
            worktree_state: "clean".to_string(),
            import_state: "clean".to_string(),
            mapping_state: "clean".to_string(),
            remote_drift: status.to_string(),
            active_operation: None,
            default_remote: Some("origin".to_string()),
            clone_verification: "not_applicable".to_string(),
            machine_contract: "available".to_string(),
            machine_contract_coverage: machine_contract_coverage(),
            workflow_status: "clean".to_string(),
            workflow_summary: "workflow fixture".to_string(),
            summary: "repository verification fixture".to_string(),
            recommended_action: if verified {
                String::new()
            } else {
                recommended_action.to_string()
            },
            recommended_action_template: None,
            recovery_commands: if verified {
                Vec::new()
            } else {
                vec![recommended_action.to_string()]
            },
            recovery_action_templates: Vec::new(),
            checks: vec![check],
        }
    }
}
