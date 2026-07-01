// SPDX-License-Identifier: Apache-2.0
//! Command-oriented state resolution with explicit per-caller policy.

use objects::{
    error::{HeddleError, Result as HeddleResult},
    object::ChangeId,
};

use super::Repository;

/// Bootstrap hook invoked when [`ResolvePolicy::bootstrap_on_empty_head`] is
/// set and `HEAD` / `@` resolves against an empty current state.
pub type EmptyHeadBootstrap<'a> = dyn Fn(&Repository) -> HeddleResult<()> + 'a;

/// Policy knobs derived from the six pre-consolidation call sites.
///
/// * `git_overlay_import_hints` — history_target + context (via the rich
///   resolver): when lookup returns `None`, check tip-only Git-overlay
///   branch/tag refs and surface a structured import-history failure.
/// * `bootstrap_on_empty_head` — context only: before resolving `HEAD` /
///   `@` with no current state, run the supplied bootstrap hook (typically
///   `ensure_current_state` in the CLI).
#[derive(Clone, Copy, Default)]
pub struct ResolvePolicy<'a> {
    pub git_overlay_import_hints: bool,
    pub bootstrap_on_empty_head: Option<&'a EmptyHeadBootstrap<'a>>,
}

impl<'a> ResolvePolicy<'a> {
    /// purge / redact / core::diff: plain lookup, no hints, no bootstrap.
    pub fn minimal() -> Self {
        Self {
            git_overlay_import_hints: false,
            bootstrap_on_empty_head: None,
        }
    }

    /// history_target and other rich CLI resolvers.
    pub fn with_git_overlay_hints() -> Self {
        Self {
            git_overlay_import_hints: true,
            bootstrap_on_empty_head: None,
        }
    }
}

/// A state spec successfully resolved for command use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedState {
    pub change_id: ChangeId,
}

/// Structured lookup failures callers map to their own user-facing text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateResolveFailure {
    NotFound { spec: String },
    GitBranchHistoryNotImported { branch: String },
    GitTagHistoryNotImported { tag: String },
}

impl std::fmt::Display for StateResolveFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { spec } => write!(f, "state not found: {spec}"),
            Self::GitBranchHistoryNotImported { branch } => {
                write!(f, "git branch history not imported: {branch}")
            }
            Self::GitTagHistoryNotImported { tag } => {
                write!(f, "git tag history not imported: {tag}")
            }
        }
    }
}

impl std::error::Error for StateResolveFailure {}

/// Full error surface for command state resolution.
#[derive(Debug)]
pub enum StateResolveError {
    Repository(HeddleError),
    Failure(StateResolveFailure),
}

impl std::fmt::Display for StateResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Repository(err) => write!(f, "{err}"),
            Self::Failure(failure) => write!(f, "{failure}"),
        }
    }
}

impl std::error::Error for StateResolveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Repository(err) => Some(err),
            Self::Failure(failure) => Some(failure),
        }
    }
}

impl From<HeddleError> for StateResolveError {
    fn from(value: HeddleError) -> Self {
        Self::Repository(value)
    }
}

/// Resolve a state specifier for command execution under explicit policy.
///
/// Ambiguous short-prefix conflicts from [`Repository::resolve_state`] still
/// surface as [`HeddleError::Conflict`] via [`StateResolveError::Repository`].
pub fn resolve_state_for_command(
    repo: &Repository,
    spec: &str,
    policy: ResolvePolicy<'_>,
) -> Result<ResolvedState, StateResolveError> {
    if let Some(bootstrap) = policy.bootstrap_on_empty_head
        && matches!(spec, "HEAD" | "@")
        && repo.current_state()?.is_none()
    {
        bootstrap(repo)?;
    }

    match repo.resolve_state(spec)? {
        Some(change_id) => Ok(ResolvedState { change_id }),
        None => resolve_missing_state(repo, spec, policy),
    }
}

fn resolve_missing_state(
    repo: &Repository,
    spec: &str,
    policy: ResolvePolicy<'_>,
) -> Result<ResolvedState, StateResolveError> {
    if policy.git_overlay_import_hints {
        if let Some(tip) = repo.git_overlay_branch_tip(spec)?
            && !tip.history_imported
        {
            return Err(StateResolveError::Failure(
                StateResolveFailure::GitBranchHistoryNotImported {
                    branch: tip.branch,
                },
            ));
        }
        if let Some(tip) = repo.git_overlay_tag_tip(spec)?
            && !tip.history_imported
        {
            return Err(StateResolveError::Failure(
                StateResolveFailure::GitTagHistoryNotImported { tag: tip.tag },
            ));
        }
    }

    Err(StateResolveError::Failure(StateResolveFailure::NotFound {
        spec: spec.to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use tempfile::TempDir;

    use super::*;
    use crate::Repository;

    fn repo_with_snapshot() -> (TempDir, Repository, ChangeId) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("a.txt"), "a").unwrap();
        let state = repo.snapshot(Some("first".into()), None).unwrap();
        (temp, repo, state.change_id)
    }

    #[test]
    fn minimal_policy_resolves_known_state() {
        let (_temp, repo, id) = repo_with_snapshot();
        let resolved =
            resolve_state_for_command(&repo, &id.to_string_full(), ResolvePolicy::minimal())
                .unwrap();
        assert_eq!(resolved.change_id, id);
    }

    #[test]
    fn minimal_policy_returns_not_found_without_hints() {
        let (_temp, repo, _) = repo_with_snapshot();
        let err = resolve_state_for_command(&repo, "hd-zzzzzzzzzzzz", ResolvePolicy::minimal())
            .unwrap_err();
        assert!(matches!(
            err,
            StateResolveError::Failure(StateResolveFailure::NotFound { .. })
        ));
    }

    #[test]
    fn bootstrap_runs_only_for_empty_head_spec() {
        let (_temp, repo, id) = repo_with_snapshot();
        let called = AtomicBool::new(false);
        let bootstrap = |_: &Repository| {
            called.store(true, Ordering::SeqCst);
            Ok(())
        };
        let policy = ResolvePolicy {
            git_overlay_import_hints: false,
            bootstrap_on_empty_head: Some(&bootstrap),
        };

        let resolved = resolve_state_for_command(&repo, "HEAD", policy).unwrap();
        assert_eq!(resolved.change_id, id);
        assert!(!called.load(Ordering::SeqCst));

        let resolved = resolve_state_for_command(&repo, &id.to_string_full(), policy).unwrap();
        assert_eq!(resolved.change_id, id);
        assert!(!called.load(Ordering::SeqCst));
    }

    #[test]
    fn bootstrap_runs_for_empty_head_before_resolving_head() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        assert!(repo.current_state().unwrap().is_none());
        std::fs::write(temp.path().join("a.txt"), "a").unwrap();

        let bootstrapped = AtomicBool::new(false);
        let bootstrap = |repo: &Repository| {
            bootstrapped.store(true, Ordering::SeqCst);
            repo.snapshot(Some("bootstrap".into()), None).map(|_| ())
        };
        let policy = ResolvePolicy {
            git_overlay_import_hints: false,
            bootstrap_on_empty_head: Some(&bootstrap),
        };

        let resolved = resolve_state_for_command(&repo, "HEAD", policy).unwrap();
        assert!(bootstrapped.load(Ordering::SeqCst));
        assert_eq!(repo.head().unwrap(), Some(resolved.change_id));
    }

    #[test]
    fn git_overlay_hint_policy_distinguishes_not_found() {
        let (_temp, repo, _) = repo_with_snapshot();
        let err = resolve_state_for_command(
            &repo,
            "missing-branch",
            ResolvePolicy::with_git_overlay_hints(),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            StateResolveError::Failure(StateResolveFailure::NotFound { .. })
        ));
    }
}