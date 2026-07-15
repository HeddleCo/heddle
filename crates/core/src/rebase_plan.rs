// SPDX-License-Identifier: Apache-2.0
//! Pure rebase preflight planning (no FS / worktree I/O).

/// Pure rebase start request facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseStartFacts {
    /// Explicit target thread name (after CLI defaulting).
    pub target_thread: Option<String>,
    /// Whether that thread exists in the repo.
    pub target_exists: bool,
}

/// Pure plan for starting a rebase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseStartPlan {
    Proceed { target_thread: String },
    TargetRequired,
    TargetNotFound { target_thread: String },
}

/// Plan rebase start from pure facts.
pub fn plan_rebase_start(facts: &RebaseStartFacts) -> RebaseStartPlan {
    let Some(target) = facts
        .target_thread
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return RebaseStartPlan::TargetRequired;
    };
    if !facts.target_exists {
        return RebaseStartPlan::TargetNotFound {
            target_thread: target.to_string(),
        };
    }
    RebaseStartPlan::Proceed {
        target_thread: target.to_string(),
    }
}

/// Pure continue/abort preflight: rebase state must exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebaseContinuePlan {
    Proceed,
    NoRebaseInProgress,
}

pub fn plan_rebase_continue(rebase_state_exists: bool) -> RebaseContinuePlan {
    if rebase_state_exists {
        RebaseContinuePlan::Proceed
    } else {
        RebaseContinuePlan::NoRebaseInProgress
    }
}

pub fn plan_rebase_abort(rebase_state_exists: bool) -> RebaseContinuePlan {
    plan_rebase_continue(rebase_state_exists)
}

/// Advice kind tokens (CLI maps to RecoveryAdvice).
pub fn no_rebase_in_progress_kind() -> &'static str {
    "no_rebase_in_progress"
}

pub fn rebase_target_required_kind() -> &'static str {
    "rebase_target_required"
}

pub fn rebase_target_not_found_kind() -> &'static str {
    "rebase_target_not_found"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebase_start_and_continue() {
        assert_eq!(
            plan_rebase_start(&RebaseStartFacts {
                target_thread: None,
                target_exists: false,
            }),
            RebaseStartPlan::TargetRequired
        );
        assert_eq!(
            plan_rebase_start(&RebaseStartFacts {
                target_thread: Some("feature".into()),
                target_exists: false,
            }),
            RebaseStartPlan::TargetNotFound {
                target_thread: "feature".into()
            }
        );
        assert_eq!(
            plan_rebase_start(&RebaseStartFacts {
                target_thread: Some("feature".into()),
                target_exists: true,
            }),
            RebaseStartPlan::Proceed {
                target_thread: "feature".into()
            }
        );
        assert_eq!(
            plan_rebase_continue(false),
            RebaseContinuePlan::NoRebaseInProgress
        );
        assert_eq!(plan_rebase_abort(true), RebaseContinuePlan::Proceed);
    }
}
