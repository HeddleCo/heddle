// SPDX-License-Identifier: Apache-2.0
//! Shared commands for the ready -> land thread landing loop.

use super::command_catalog::{heddle_action, thread_flag_args};

pub(crate) fn merge_preview_command(thread_id: &str) -> String {
    let mut argv = vec!["ready".to_string()];
    argv.extend(thread_flag_args(thread_id));
    heddle_action(argv)
}

pub(crate) fn switch_thread_command(thread_id: &str) -> String {
    heddle_action(["thread", "switch", thread_id])
}

pub(crate) fn land_command_for_thread(_repo: &repo::Repository, thread_id: &str) -> String {
    land_local_command(thread_id)
}

pub(crate) fn land_local_command(thread_id: &str) -> String {
    let mut argv = vec!["land".to_string()];
    argv.extend(thread_flag_args(thread_id));
    heddle_action(argv)
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
        assert_eq!(
            merge_preview_command("feature with spaces"),
            "heddle ready --thread 'feature with spaces'"
        );
    }

    // heddle#464 close-the-class (r9): a historical `-foo` thread id (un-creatable
    // now, but reachable via `new_unchecked` deserialization) must yield a
    // clap-valid command from EVERY land/merge breadcrumb builder. `heddle_action`
    // validates through clap and panics on an invalid action, so a builder still
    // emitting the bare `--thread -foo` (or positional `-foo`) form would panic
    // here — this test is the conformance gate against the next sibling.
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
        // `cli::render::shell_quote` (used by checked_action_from_argv) treats
        // `=` as unsafe, so `--thread=-foo` renders single-quoted — still
        // runnable (the shell strips the quotes, clap binds the value) and, as
        // the loop above asserts, accepted by the validator.
        assert_eq!(land_local_command(id), "heddle land '--thread=-foo'");
        assert_eq!(merge_preview_command(id), "heddle ready '--thread=-foo'");
        assert_eq!(switch_thread_command(id), "heddle thread switch -foo");
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
