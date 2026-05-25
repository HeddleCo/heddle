// SPDX-License-Identifier: Apache-2.0
//! Shared next-action selection for command surfaces.

use repo::{GitOverlayImportHint, GitRemoteTrackingStatus, RepositoryOperationStatus};

use super::git_overlay_health::{RepositoryVerificationState, import_hint_includes_active_branch};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NextActionScope {
    Default,
    CurrentThread,
    Ready,
}

pub(crate) struct NextActionInput<'a> {
    pub operation: Option<&'a RepositoryOperationStatus>,
    pub remote_tracking: Option<&'a GitRemoteTrackingStatus>,
    pub import_hint: Option<&'a GitOverlayImportHint>,
    pub fallback: Option<&'a str>,
    pub thread_health: Option<&'a str>,
    pub trust: Option<&'a RepositoryVerificationState>,
    pub scope: NextActionScope,
}

impl<'a> NextActionInput<'a> {
    pub(crate) fn default(
        operation: Option<&'a RepositoryOperationStatus>,
        remote_tracking: Option<&'a GitRemoteTrackingStatus>,
        import_hint: Option<&'a GitOverlayImportHint>,
        fallback: Option<&'a str>,
    ) -> Self {
        Self {
            operation,
            remote_tracking,
            import_hint,
            fallback,
            thread_health: None,
            trust: None,
            scope: NextActionScope::Default,
        }
    }

    pub(crate) fn with_verification(mut self, trust: &'a RepositoryVerificationState) -> Self {
        self.trust = Some(trust);
        self
    }

    pub(crate) fn current_thread(mut self, thread_health: Option<&'a str>) -> Self {
        self.thread_health = thread_health;
        self.scope = NextActionScope::CurrentThread;
        self
    }

    pub(crate) fn ready(mut self) -> Self {
        self.scope = NextActionScope::Ready;
        self
    }
}

pub(crate) fn effective_next_action(input: NextActionInput<'_>) -> String {
    if let Some(trust) = input.trust
        && !trust.verified
    {
        return trust.recommended_action.clone();
    }

    match input.scope {
        NextActionScope::Ready => ready_next_action(input),
        NextActionScope::CurrentThread => current_thread_next_action(input),
        NextActionScope::Default => default_next_action(input),
    }
}

fn ready_next_action(input: NextActionInput<'_>) -> String {
    if let Some(operation) = input.operation {
        return operation.next_action.clone();
    }
    if let Some(action) = non_empty_action(input.fallback) {
        return action.to_string();
    }
    default_next_action(NextActionInput {
        operation: None,
        remote_tracking: input.remote_tracking,
        import_hint: input.import_hint,
        fallback: None,
        thread_health: None,
        trust: None,
        scope: NextActionScope::Default,
    })
}

fn current_thread_next_action(input: NextActionInput<'_>) -> String {
    let thread_action = non_empty_action(input.fallback);
    if input.operation.is_none()
        && thread_recovery_precedes_publish(
            input.remote_tracking,
            input.thread_health,
            thread_action,
        )
    {
        return thread_action.unwrap_or_default().to_string();
    }
    default_next_action(NextActionInput {
        operation: input.operation,
        remote_tracking: input.remote_tracking,
        import_hint: input.import_hint,
        fallback: thread_action,
        thread_health: None,
        trust: None,
        scope: NextActionScope::Default,
    })
}

fn default_next_action(input: NextActionInput<'_>) -> String {
    if let Some(operation) = input.operation {
        return operation.next_action.clone();
    }
    if let Some(remote_tracking) = input.remote_tracking {
        if remote_tracking.behind > 0 {
            return if remote_tracking.ahead > 0 {
                if remote_tracking.upstream.is_empty() {
                    "heddle fetch".to_string()
                } else {
                    format!(
                        "heddle bridge git import --ref {}",
                        remote_tracking.upstream
                    )
                }
            } else {
                "heddle pull".to_string()
            };
        }
        return "heddle push".to_string();
    }
    if let Some(action) = non_empty_action(input.fallback) {
        return action.to_string();
    }
    if let Some(hint) = input.import_hint
        && import_hint_includes_active_branch(hint)
    {
        return hint.recommended_command.clone();
    }
    String::new()
}

pub(crate) fn thread_recovery_precedes_publish(
    remote_tracking: Option<&GitRemoteTrackingStatus>,
    thread_health: Option<&str>,
    thread_action: Option<&str>,
) -> bool {
    let Some(remote_tracking) = remote_tracking else {
        return false;
    };
    if remote_tracking.ahead == 0 || remote_tracking.behind > 0 {
        return false;
    }
    let Some(thread_action) = thread_action else {
        return false;
    };
    thread_recovery_action_is_primary(thread_health, thread_action)
}

pub(crate) fn thread_recovery_action_is_primary(
    thread_health: Option<&str>,
    thread_action: &str,
) -> bool {
    matches!(
        thread_health.unwrap_or_default(),
        "blocked" | "dirty_worktree" | "uncaptured"
    ) || thread_action == "heddle capture"
        || thread_action.starts_with("heddle thread refresh ")
        || thread_action.starts_with("heddle thread resolve ")
        || thread_action.starts_with("heddle thread promote ")
}

fn non_empty_action(action: Option<&str>) -> Option<&str> {
    action.filter(|action| !action.trim().is_empty())
}
