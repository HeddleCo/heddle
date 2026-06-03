// SPDX-License-Identifier: Apache-2.0
//! Shared commands for the ready -> land thread landing loop.

use repo::Repository;

use super::command_catalog::heddle_action;

pub(crate) fn merge_preview_command(thread_id: &str) -> String {
    heddle_action(["merge", thread_id, "--preview"])
}

pub(crate) fn land_command_for_thread(repo: &Repository, thread_id: &str) -> String {
    let has_push_target = super::remote::resolved_default_remote_name(repo)
        .ok()
        .flatten()
        .is_some();
    land_command_with_push_target(thread_id, has_push_target)
}

pub(crate) fn land_command_with_push_target(thread_id: &str, has_push_target: bool) -> String {
    if has_push_target {
        land_push_command(thread_id)
    } else {
        land_local_command(thread_id)
    }
}

pub(crate) fn land_push_command(thread_id: &str) -> String {
    heddle_action(["land", "--thread", thread_id, "--push"])
}

pub(crate) fn land_push_remote_command(thread_id: &str, remote: &str) -> String {
    heddle_action(["land", "--thread", thread_id, "--push", "--remote", remote])
}

pub(crate) fn land_local_command(thread_id: &str) -> String {
    heddle_action(["land", "--thread", thread_id, "--no-push"])
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
    if action == merge_preview_command(thread_id) {
        return heddle_action(vec![
            "--repo".to_string(),
            main_root.display().to_string(),
            "merge".to_string(),
            thread_id.to_string(),
            "--preview".to_string(),
        ]);
    }
    if action == land_local_command(thread_id) {
        return heddle_action(vec![
            "--repo".to_string(),
            main_root.display().to_string(),
            "land".to_string(),
            "--thread".to_string(),
            thread_id.to_string(),
            "--no-push".to_string(),
        ]);
    }
    if action == land_push_command(thread_id) {
        return heddle_action(vec![
            "--repo".to_string(),
            main_root.display().to_string(),
            "land".to_string(),
            "--thread".to_string(),
            thread_id.to_string(),
            "--push".to_string(),
        ]);
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
            land_local_command("feature/demo"),
            "heddle land --thread feature/demo --no-push"
        );
        assert_eq!(
            land_command_with_push_target("feature/demo", true),
            "heddle land --thread feature/demo --push"
        );
        assert_eq!(
            land_push_remote_command("feature/demo", "origin"),
            "heddle land --thread feature/demo --push --remote origin"
        );
        assert_eq!(
            merge_preview_command("feature with spaces"),
            "heddle merge 'feature with spaces' --preview"
        );
    }
}
