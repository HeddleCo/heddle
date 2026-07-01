// SPDX-License-Identifier: Apache-2.0
//! Shared status and command next-action selection.

use repo::{GitOverlayImportHint, GitRemoteTrackingStatus, RepositoryOperationStatus, shell_quote};

use crate::RepositoryVerificationState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextActionScope {
    Default,
    CurrentThread,
    Ready,
}

pub struct NextActionInput<'a> {
    pub operation: Option<&'a RepositoryOperationStatus>,
    pub remote_tracking: Option<&'a GitRemoteTrackingStatus>,
    pub import_hint: Option<&'a GitOverlayImportHint>,
    pub fallback: Option<&'a str>,
    pub thread_health: Option<&'a str>,
    pub trust: Option<&'a RepositoryVerificationState>,
    pub scope: NextActionScope,
}

impl<'a> NextActionInput<'a> {
    pub fn default(
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

    pub fn with_verification(mut self, trust: &'a RepositoryVerificationState) -> Self {
        self.trust = Some(trust);
        self
    }

    pub fn current_thread(mut self, thread_health: Option<&'a str>) -> Self {
        self.thread_health = thread_health;
        self.scope = NextActionScope::CurrentThread;
        self
    }

    pub fn ready(mut self) -> Self {
        self.scope = NextActionScope::Ready;
        self
    }
}

pub fn effective_next_action(input: NextActionInput<'_>) -> String {
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
        && thread_recovery_precedes_publish(input.remote_tracking, input.thread_health, thread_action)
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
    if let Some(remote_tracking) = input.remote_tracking
        && let Some(action) = remote_tracking_next_action(remote_tracking)
    {
        return action;
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

pub fn remote_tracking_status(remote: &GitRemoteTrackingStatus) -> &'static str {
    if remote.upstream.is_empty() {
        return "remote_untracked";
    }
    if remote.upstream_is_undone_checkpoint && remote.ahead == 0 && remote.behind > 0 {
        return "remote_contains_undone_checkpoint";
    }
    match (remote.ahead, remote.behind) {
        (0, 0) => "clean",
        (0, _) => "remote_behind",
        (_, 0) => "remote_ahead",
        _ => "remote_diverged",
    }
}

pub fn remote_tracking_next_action(remote: &GitRemoteTrackingStatus) -> Option<String> {
    match remote_tracking_status(remote) {
        "clean" => None,
        "remote_untracked" => Some(remote_untracked_action(remote)),
        "remote_contains_undone_checkpoint" => Some(heddle_action(["push", "--force"])),
        "remote_behind" => Some("heddle pull".to_string()),
        "remote_ahead" => Some("heddle push".to_string()),
        "remote_diverged" => {
            let upstream = remote.upstream.trim();
            if upstream.is_empty() {
                Some("heddle fetch".to_string())
            } else {
                Some(canonical_bridge_import_ref_command(upstream))
            }
        }
        _ => None,
    }
}

pub fn remote_untracked_action(remote: &GitRemoteTrackingStatus) -> String {
    if remote.next_action.trim().is_empty() {
        "heddle push".to_string()
    } else {
        remote.next_action.clone()
    }
}

pub fn canonical_adopt_ref_command(ref_name: &str) -> String {
    heddle_action(["adopt", "--ref", ref_name])
}

pub fn canonical_bridge_import_ref_command(ref_name: &str) -> String {
    heddle_action(["bridge", "git", "import", "--ref", ref_name])
}

pub fn canonical_bridge_reconcile_ref_preview_command(
    prefer: Option<&str>,
    ref_name: &str,
) -> String {
    match prefer {
        Some(prefer) => heddle_action([
            "bridge",
            "git",
            "reconcile",
            "--prefer",
            prefer,
            "--ref",
            ref_name,
            "--preview",
        ]),
        None => heddle_action(["bridge", "git", "reconcile", "--ref", ref_name, "--preview"]),
    }
}

pub fn canonical_bridge_reconcile_ref_command(prefer: &str, ref_name: &str) -> String {
    heddle_action([
        "bridge",
        "git",
        "reconcile",
        "--prefer",
        prefer,
        "--ref",
        ref_name,
    ])
}

pub fn heddle_action<I, S>(args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    std::iter::once("heddle".to_string())
        .chain(args.into_iter().map(|arg| shell_quote(arg.as_ref())))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn import_hint_includes_active_branch(hint: &GitOverlayImportHint) -> bool {
    hint.missing_branches
        .iter()
        .any(|branch| branch == &hint.current_branch)
}

pub fn thread_recovery_precedes_publish(
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

pub fn thread_recovery_action_is_primary(thread_health: Option<&str>, thread_action: &str) -> bool {
    matches!(
        thread_health.unwrap_or_default(),
        "blocked" | "dirty_worktree" | "uncaptured"
    ) || thread_action == "heddle commit"
        || thread_action.starts_with("heddle commit ")
        || thread_action.starts_with("heddle sync ")
        || thread_action.starts_with("heddle resolve ")
        || thread_action.starts_with("heddle thread promote ")
}

pub fn non_empty_action(action: Option<&str>) -> Option<&str> {
    action.filter(|action| !action.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(
        branch: &str,
        upstream: &str,
        ahead: usize,
        behind: usize,
        next_action: &str,
    ) -> GitRemoteTrackingStatus {
        GitRemoteTrackingStatus {
            branch: branch.to_string(),
            upstream: upstream.to_string(),
            ahead,
            behind,
            local_oid: Some("head".to_string()),
            upstream_oid: Some("upstream".to_string()),
            upstream_is_undone_checkpoint: false,
            message: String::new(),
            next_action: next_action.to_string(),
        }
    }

    #[test]
    fn remote_tracking_next_action_covers_basic_git_states_without_repo_context() {
        assert_eq!(
            remote_tracking_next_action(&remote("main", "origin/main", 0, 1, "heddle pull"))
                .as_deref(),
            Some("heddle pull")
        );
        assert_eq!(
            remote_tracking_next_action(&remote("main", "origin/main", 1, 0, "heddle push"))
                .as_deref(),
            Some("heddle push")
        );
        assert_eq!(
            remote_tracking_next_action(&remote("main", "origin/main", 1, 1, "heddle fetch"))
                .as_deref(),
            Some("heddle bridge git import --ref origin/main")
        );
        assert_eq!(
            remote_tracking_next_action(&remote("main", "", 1, 0, "heddle push")).as_deref(),
            Some("heddle push")
        );
    }

    #[test]
    fn current_thread_recovery_precedes_publish_when_thread_action_is_primary() {
        let remote = remote("feature", "origin/feature", 1, 0, "heddle push");
        let action = effective_next_action(
            NextActionInput::default(
                None,
                Some(&remote),
                None,
                Some("heddle commit -m \"...\""),
            )
            .current_thread(Some("dirty_worktree")),
        );
        assert_eq!(action, "heddle commit -m \"...\"");
    }

    #[test]
    fn ready_scope_prefers_thread_action_before_publish() {
        let remote = remote("feature", "origin/feature", 1, 0, "heddle push");
        let action = effective_next_action(
            NextActionInput::default(None, Some(&remote), None, Some("heddle land --thread f"))
                .ready(),
        );
        assert_eq!(action, "heddle land --thread f");
    }
}
