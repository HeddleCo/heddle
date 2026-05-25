// SPDX-License-Identifier: Apache-2.0
//! Canonical state-ID resolver used by every CLI verb that takes a
//! state argument.
//!
//! A "state spec" is anything a user is likely to write to identify a
//! captured state:
//!
//! * a full change ID (`hd-sqr398dvx9ayt9bf8bf5gz0jg8`)
//! * a short change ID prefix (`hd-sqr398dvx9ay`, as printed by
//!   `heddle log --output json`)
//! * a marker name (`failed-build-2026-05-09`)
//! * `HEAD`, `@`, `HEAD~N`, `@~N`
//! * a thread name
//!
//! The hard work happens in [`Repository::resolve_state`]; this layer
//! adds two pieces of CLI-friendly polish:
//!
//! 1. A precise "not found" error message instead of `Ok(None)`.
//! 2. A targeted hint when the spec matches a tip-only Git overlay
//!    branch/tag whose history hasn't been imported yet.
//!
//! Use [`resolve_state_id`] for the typed [`ChangeId`]. Use
//! [`resolve_state_id_bytes`] when you need the wire-form 16-byte
//! representation (e.g. when handing it to a gRPC service stub).

use anyhow::{Result, anyhow};
use objects::object::ChangeId;
use repo::Repository;

use super::advice::RecoveryAdvice;

/// Resolve a state spec to a typed [`ChangeId`].
///
/// Errors are user-facing:
/// * `"State not found: <spec>"` when nothing matches.
/// * `"ambiguous state ID prefix '<spec>' matches: <list>"` when a short
///   prefix matches more than one state.
/// * A targeted import-history hint when the spec matches a tip-only
///   Git-overlay ref whose history we haven't pulled yet.
pub(crate) fn resolve_state_id(repo: &Repository, spec: &str) -> Result<ChangeId> {
    match repo.resolve_state(spec)? {
        Some(id) => Ok(id),
        None => {
            if let Some(tip) = repo.git_overlay_branch_tip(spec)?
                && !tip.history_imported
            {
                return Err(anyhow!(tip_only_branch_history_advice(&tip.branch)));
            }
            if let Some(tip) = repo.git_overlay_tag_tip(spec)?
                && !tip.history_imported
            {
                return Err(anyhow!(tip_only_tag_history_advice(&tip.tag)));
            }
            Err(anyhow!(state_not_found_advice(spec)))
        }
    }
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
    let import_command = super::git_overlay_health::canonical_adopt_ref_command(branch);
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
    let import_command = super::git_overlay_health::canonical_adopt_ref_command(tag);
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
