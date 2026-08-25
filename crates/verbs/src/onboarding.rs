// SPDX-License-Identifier: Apache-2.0
//! Pure repository-onboarding classification.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingRepositoryState {
    NativeUninitialized,
    PlainGitUnborn,
    PlainGitCommitted,
    NativeInitialized,
    GitOverlayInitialized,
}

impl OnboardingRepositoryState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NativeUninitialized => "native_uninitialized",
            Self::PlainGitUnborn => "plain_git_unborn",
            Self::PlainGitCommitted => "plain_git_committed",
            Self::NativeInitialized => "native_initialized",
            Self::GitOverlayInitialized => "git_overlay_initialized",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingMode {
    Native,
    GitOverlay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingAction {
    Init,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OnboardingFacts {
    pub git_worktree: bool,
    pub git_has_commits: bool,
    pub heddle_mode: Option<OnboardingMode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OnboardingPlan {
    pub state: OnboardingRepositoryState,
    pub mode: OnboardingMode,
    pub action: OnboardingAction,
}

impl OnboardingPlan {
    pub fn recommended_command(self) -> Option<&'static str> {
        match self.action {
            OnboardingAction::Init => Some("heddle init"),
            OnboardingAction::None => None,
        }
    }

    pub fn storage_summary(self) -> &'static str {
        match self.mode {
            OnboardingMode::Native => "Heddle owns source objects, refs, and worktree state",
            OnboardingMode::GitOverlay => {
                "Git owns source objects, refs, index, and worktree state; Heddle owns metadata, provenance, and projection mapping"
            }
        }
    }
}

pub fn plan_repository_onboarding(facts: OnboardingFacts) -> OnboardingPlan {
    if let Some(mode) = facts.heddle_mode {
        let state = match mode {
            OnboardingMode::Native => OnboardingRepositoryState::NativeInitialized,
            OnboardingMode::GitOverlay => OnboardingRepositoryState::GitOverlayInitialized,
        };
        return OnboardingPlan {
            state,
            mode,
            action: OnboardingAction::None,
        };
    }

    if facts.git_worktree {
        return OnboardingPlan {
            state: if facts.git_has_commits {
                OnboardingRepositoryState::PlainGitCommitted
            } else {
                OnboardingRepositoryState::PlainGitUnborn
            },
            mode: OnboardingMode::GitOverlay,
            action: OnboardingAction::Init,
        };
    }

    OnboardingPlan {
        state: OnboardingRepositoryState::NativeUninitialized,
        mode: OnboardingMode::Native,
        action: OnboardingAction::Init,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_uninitialized_repositories() {
        let committed = plan_repository_onboarding(OnboardingFacts {
            git_worktree: true,
            git_has_commits: true,
            heddle_mode: None,
        });
        assert_eq!(
            committed.state,
            OnboardingRepositoryState::PlainGitCommitted
        );
        assert_eq!(committed.mode, OnboardingMode::GitOverlay);
        assert_eq!(committed.recommended_command(), Some("heddle init"));

        let unborn = plan_repository_onboarding(OnboardingFacts {
            git_worktree: true,
            git_has_commits: false,
            heddle_mode: None,
        });
        assert_eq!(unborn.state, OnboardingRepositoryState::PlainGitUnborn);
        assert_eq!(unborn.mode, OnboardingMode::GitOverlay);
        assert_eq!(unborn.recommended_command(), Some("heddle init"));

        let native = plan_repository_onboarding(OnboardingFacts {
            git_worktree: false,
            git_has_commits: false,
            heddle_mode: None,
        });
        assert_eq!(native.state, OnboardingRepositoryState::NativeUninitialized);
        assert_eq!(native.mode, OnboardingMode::Native);
        assert_eq!(native.recommended_command(), Some("heddle init"));
    }

    #[test]
    fn initialized_overlay_needs_no_onboarding_action() {
        let plan = plan_repository_onboarding(OnboardingFacts {
            git_worktree: true,
            git_has_commits: true,
            heddle_mode: Some(OnboardingMode::GitOverlay),
        });
        assert_eq!(plan.state, OnboardingRepositoryState::GitOverlayInitialized);
        assert_eq!(plan.action, OnboardingAction::None);
        assert_eq!(plan.recommended_command(), None);
    }
}
