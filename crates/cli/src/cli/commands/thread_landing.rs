// SPDX-License-Identifier: Apache-2.0
//! Shared commands for the ready -> preview -> ship thread landing loop.

use repo::Repository;

use crate::cli::render::shell_quote;

pub(crate) fn merge_preview_command(thread_id: &str) -> String {
    format!("heddle merge {} --preview", shell_quote(thread_id))
}

pub(crate) fn ship_command_for_thread(repo: &Repository, thread_id: &str) -> String {
    let has_push_target = super::remote::resolved_default_remote_name(repo)
        .ok()
        .flatten()
        .is_some();
    ship_command_with_push_target(thread_id, has_push_target)
}

pub(crate) fn ship_command_with_push_target(thread_id: &str, has_push_target: bool) -> String {
    if has_push_target {
        ship_push_command(thread_id)
    } else {
        ship_local_command(thread_id)
    }
}

pub(crate) fn ship_push_command(thread_id: &str) -> String {
    format!("heddle ship --thread {} --push", shell_quote(thread_id))
}

pub(crate) fn ship_push_remote_command(thread_id: &str, remote: &str) -> String {
    format!(
        "heddle ship --thread {} --push --remote {}",
        shell_quote(thread_id),
        shell_quote(remote)
    )
}

pub(crate) fn ship_local_command(thread_id: &str) -> String {
    format!("heddle ship --thread {} --no-push", shell_quote(thread_id))
}

pub(crate) fn contextual_thread_action(
    repo: &Repository,
    thread_id: &str,
    target_thread: Option<&str>,
    action: &str,
) -> String {
    let Some(main_root) = repo.heddle_dir().parent() else {
        return action.to_string();
    };
    if main_root == repo.root() || target_thread.is_none() {
        return action.to_string();
    }
    let repo_arg = shell_quote(&main_root.display().to_string());
    let thread_arg = shell_quote(thread_id);
    if action == merge_preview_command(thread_id) {
        return format!("heddle --repo {repo_arg} merge {thread_arg} --preview");
    }
    if action == ship_local_command(thread_id) {
        return format!("heddle --repo {repo_arg} ship --thread {thread_arg} --no-push");
    }
    if action == ship_push_command(thread_id) {
        return format!("heddle --repo {repo_arg} ship --thread {thread_arg} --push");
    }
    action.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn landing_commands_are_stable_and_copy_pasteable() {
        assert_eq!(
            merge_preview_command("feature/demo"),
            "heddle merge feature/demo --preview"
        );
        assert_eq!(
            ship_local_command("feature/demo"),
            "heddle ship --thread feature/demo --no-push"
        );
        assert_eq!(
            ship_command_with_push_target("feature/demo", true),
            "heddle ship --thread feature/demo --push"
        );
        assert_eq!(
            ship_push_remote_command("feature/demo", "origin"),
            "heddle ship --thread feature/demo --push --remote origin"
        );
        assert_eq!(
            merge_preview_command("feature with spaces"),
            "heddle merge 'feature with spaces' --preview"
        );
    }
}
