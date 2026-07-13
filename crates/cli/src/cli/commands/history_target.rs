// SPDX-License-Identifier: Apache-2.0
//! Canonical state-ID resolver used by every CLI verb that takes a
//! state argument.
//!
//! A "state spec" is anything a user is likely to write to identify a
//! captured state:
//!
//! * a full state ID (`hs-sqr398dvx9ayt9bf8bf5gz0jg8`)
//! * a short state ID prefix (`hs-sqr398dvx9ay`, as printed by
//!   `heddle log --output json`)
//! * a marker name (`failed-build-2026-05-09`)
//! * `HEAD`, `@`, `HEAD~N`, `@~N`
//! * a thread name
//!
//! Resolution policy and lookup live in [`repo::resolve_state_for_command`];
//! this layer maps structured failures to CLI [`RecoveryAdvice`] envelopes.
//!
//! Use [`resolve_state_id`] for the typed [`StateId`]. Use
//! [`resolve_state_id_bytes`] when you need the wire-form 16-byte
//! representation (e.g. when handing it to a gRPC service stub).

use anyhow::{Result, anyhow};
use heddle_core::status::next_action::canonical_git_import_ref_command;
use objects::{
    error::HeddleError,
    object::{State, StateId},
    store::ObjectStore,
};
use repo::{Repository, ResolvePolicy, StateResolveFailure, resolve_state_for_command};

use super::advice::RecoveryAdvice;

pub(crate) fn state_resolve_failure_to_error(failure: StateResolveFailure) -> anyhow::Error {
    match failure {
        StateResolveFailure::GitBranchHistoryNotImported { branch } => {
            anyhow!(tip_only_branch_history_advice(&branch))
        }
        StateResolveFailure::GitTagHistoryNotImported { tag } => {
            anyhow!(tip_only_tag_history_advice(&tag))
        }
        StateResolveFailure::NotFound { spec } => anyhow!(state_not_found_advice(&spec)),
    }
}

/// Resolve a state spec to a typed [`StateId`].
///
/// Errors are user-facing:
/// * `"State not found: <spec>"` when nothing matches.
/// * `"ambiguous state ID prefix '<spec>' matches: <list>"` when a short
///   prefix matches more than one state.
/// * A targeted import-history hint when the spec matches a tip-only
///   Git-overlay ref whose history we haven't pulled yet.
pub(crate) fn resolve_state_id(repo: &Repository, spec: &str) -> Result<StateId> {
    resolve_state_id_with_policy(repo, spec, ResolvePolicy::with_git_overlay_hints())
}

pub(crate) fn resolve_state_id_with_policy(
    repo: &Repository,
    spec: &str,
    policy: ResolvePolicy<'_>,
) -> Result<StateId> {
    resolve_state_for_command(repo, spec, policy)
        .map(|resolved| resolved.state_id)
        .map_err(state_resolve_error_to_anyhow)
}

fn state_resolve_error_to_anyhow(error: repo::StateResolveError) -> anyhow::Error {
    match error {
        repo::StateResolveError::Repository(err) => err.into(),
        repo::StateResolveError::Failure(failure) => state_resolve_failure_to_error(failure),
    }
}

/// Load a state after its ID has already resolved.
///
/// A missing object at this point is an integrity/storage problem, not
/// a user-supplied state-spec lookup failure.
pub(crate) fn require_resolved_state(repo: &Repository, id: &StateId) -> Result<State> {
    repo.store().get_state(id)?.ok_or_else(|| {
        anyhow::Error::new(HeddleError::MissingObject {
            object_type: "state".to_string(),
            id: id.to_string_full(),
        })
    })
}

fn state_not_found_advice(spec: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "state_not_found",
        format!("State not found: {spec}"),
        "Inspect available states with `heddle log`, then retry with an existing state, marker, thread, or HEAD expression.",
        format!("no Heddle state, marker, thread, or HEAD expression matched '{spec}'"),
        "the command cannot move refs, inspect content, or write worktree files until the target state is resolved",
        "repository state and worktree files were left unchanged",
        "heddle log",
        vec!["heddle log".to_string()],
    )
}

fn tip_only_branch_history_advice(branch: &str) -> RecoveryAdvice {
    let import_command = canonical_git_import_ref_command(branch);
    RecoveryAdvice::safety_refusal(
        "git_branch_history_not_imported",
        format!("Heddle has not imported Git branch '{branch}' history yet"),
        format!("Import its history first with `{import_command}`."),
        format!("branch '{branch}' has a Git tip but no imported Heddle history"),
        "history-sensitive commands cannot safely resolve this branch until Heddle imports its Git history",
        "repository state and worktree files were left unchanged",
        import_command.clone(),
        vec![import_command],
    )
}

fn tip_only_tag_history_advice(tag: &str) -> RecoveryAdvice {
    let import_command = canonical_git_import_ref_command(tag);
    RecoveryAdvice::safety_refusal(
        "git_tag_history_not_imported",
        format!("Git tag '{tag}' is visible but its history is not imported yet"),
        format!("Import it first with `{import_command}`."),
        format!("tag '{tag}' has a Git tip but no imported Heddle history"),
        "history-sensitive commands cannot safely resolve this tag to a Heddle state yet",
        "repository state and worktree files were left unchanged",
        import_command.clone(),
        vec![import_command],
    )
}

/// Resolve a state spec to its wire-form 16-byte representation.
///
/// Convenience wrapper used by services that hand state IDs across a
/// gRPC boundary. Equivalent to `resolve_state_id(...)?.as_bytes().to_vec()`.
pub(crate) fn resolve_state_id_bytes(repo: &Repository, spec: &str) -> Result<Vec<u8>> {
    Ok(resolve_state_id(repo, spec)?.as_bytes().to_vec())
}
