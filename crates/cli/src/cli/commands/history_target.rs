// SPDX-License-Identifier: Apache-2.0
//! Canonical state-ID resolver used by every CLI verb that takes a
//! state argument.
//!
//! A "state spec" is anything a user is likely to write to identify a
//! captured state:
//!
//! * a full change ID (`hd-sqr398dvx9ayt9bf8bf5gz0jg8`)
//! * a short change ID prefix (`hd-sqr398dvx9ay`, as printed by
//!   `heddle log --json`)
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
                return Err(anyhow!(
                    "Git branch '{}' is visible as a tip-only mirror. Import its history first with `heddle bridge git import --ref {}`.",
                    tip.branch,
                    tip.branch
                ));
            }
            if let Some(tip) = repo.git_overlay_tag_tip(spec)?
                && !tip.history_imported
            {
                return Err(anyhow!(
                    "Git tag '{}' is visible but its history is not imported yet. Import it first with `heddle bridge git import --ref {}`.",
                    tip.tag,
                    tip.tag
                ));
            }
            Err(anyhow!("State not found: {}", spec))
        }
    }
}

/// Resolve a state spec to its wire-form 16-byte representation.
///
/// Convenience wrapper used by services that hand state IDs across a
/// gRPC boundary. Equivalent to `resolve_state_id(...)?.as_bytes().to_vec()`.
pub(crate) fn resolve_state_id_bytes(repo: &Repository, spec: &str) -> Result<Vec<u8>> {
    Ok(resolve_state_id(repo, spec)?.as_bytes().to_vec())
}