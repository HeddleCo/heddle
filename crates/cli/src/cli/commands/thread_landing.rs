// SPDX-License-Identifier: Apache-2.0
//! Shared commands for the ready -> land thread landing loop.

use super::command_catalog::heddle_action;
pub(crate) use super::command_catalog::{land_local_command, merge_preview_command};

pub(crate) fn switch_thread_command(thread_id: &str) -> String {
    let mut argv = vec!["thread".to_string(), "switch".to_string()];
    if thread_id.starts_with('-') {
        argv.push("--".to_string());
    }
    argv.push(thread_id.to_string());
    heddle_action(argv)
}

pub(crate) fn land_command_for_thread(_repo: &repo::Repository, thread_id: &str) -> String {
    land_local_command(thread_id)
}

/// Land of the current checkout is `heddle land`. `--thread` / `--repo` are
/// invisible defaults once the caller is already on that thread (heddle#1456).
pub(crate) fn land_action_when(thread_id: &str, current_checkout: bool) -> String {
    if current_checkout {
        heddle_action(["land"])
    } else {
        land_local_command(thread_id)
    }
}

pub(crate) fn land_action_for_ready(repo: &repo::Repository, thread_id: &str) -> String {
    land_action_when(thread_id, checkout_is_thread(repo, thread_id))
}

fn checkout_is_thread(repo: &repo::Repository, thread_id: &str) -> bool {
    super::thread_cmd::current_thread(repo)
        .ok()
        .flatten()
        .is_some_and(|thread| thread.id == thread_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn landing_commands_are_stable_and_copy_pasteable() {
        assert_eq!(
            merge_preview_command("feature/demo"),
            "heddle ready --thread feature/demo"
        );
        assert_eq!(
            land_local_command("feature/demo"),
            "heddle land --thread feature/demo"
        );
        assert_eq!(land_action_when("feature/demo", true), "heddle land");
        assert_eq!(
            land_action_when("feature/demo", false),
            "heddle land --thread feature/demo"
        );
        assert_eq!(
            merge_preview_command("feature with spaces"),
            "heddle ready --thread 'feature with spaces'"
        );
    }

    #[test]
    fn land_breadcrumbs_handle_leading_dash_thread_ids() {
        use super::super::command_catalog::validate_recommended_action;
        let id = "-foo";
        let cmds = [
            land_local_command(id),
            merge_preview_command(id),
            switch_thread_command(id),
        ];
        for cmd in &cmds {
            validate_recommended_action(cmd).unwrap_or_else(|e| {
                panic!("breadcrumb `{cmd}` must validate for a leading-dash id: {e}")
            });
        }
        assert_eq!(land_action_when(id, true), "heddle land");
        validate_recommended_action("heddle land")
            .unwrap_or_else(|e| panic!("current-checkout land breadcrumb must validate: {e}"));
        assert_eq!(land_local_command(id), "heddle land '--thread=-foo'");
        assert_eq!(merge_preview_command(id), "heddle ready '--thread=-foo'");
        assert_eq!(switch_thread_command(id), "heddle thread switch -- -foo");
    }

    #[test]
    fn switch_thread_command_is_stable_and_copy_pasteable() {
        assert_eq!(
            switch_thread_command("feature/demo"),
            "heddle thread switch feature/demo"
        );
        assert_eq!(
            switch_thread_command("feature with spaces"),
            "heddle thread switch 'feature with spaces'"
        );
    }
}
