// SPDX-License-Identifier: Apache-2.0
//! Shared commands for the ready -> land thread landing loop.

use repo::Repository;

use super::command_catalog::{heddle_action, thread_flag_args};

pub(crate) fn merge_preview_command(thread_id: &str) -> String {
    // `merge` takes the thread as a POSITIONAL. A leading-dash id needs the `--`
    // end-of-options separator (positionals can't use the `=` form), so the
    // flags move ahead of it. (heddle#464 close-the-class.)
    if thread_id.starts_with('-') {
        heddle_action(vec![
            "merge".to_string(),
            "--preview".to_string(),
            "--".to_string(),
            thread_id.to_string(),
        ])
    } else {
        heddle_action(["merge", thread_id, "--preview"])
    }
}

pub(crate) fn switch_thread_command(thread_id: &str) -> String {
    // `switch` takes the thread as a POSITIONAL, so a leading-dash id needs the
    // `--` end-of-options separator (the `=` form is flag-only). (heddle#464
    // close-the-class.)
    if thread_id.starts_with('-') {
        heddle_action(vec![
            "switch".to_string(),
            "--".to_string(),
            thread_id.to_string(),
        ])
    } else {
        heddle_action(["switch", thread_id])
    }
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
    let mut argv = vec!["land".to_string()];
    argv.extend(thread_flag_args(thread_id));
    argv.push("--push".to_string());
    heddle_action(argv)
}

pub(crate) fn land_push_remote_command(thread_id: &str, remote: &str) -> String {
    let mut argv = vec!["land".to_string()];
    argv.extend(thread_flag_args(thread_id));
    argv.extend(["--push".to_string(), "--remote".to_string(), remote.to_string()]);
    heddle_action(argv)
}

pub(crate) fn land_local_command(thread_id: &str) -> String {
    let mut argv = vec!["land".to_string()];
    argv.extend(thread_flag_args(thread_id));
    argv.push("--no-push".to_string());
    heddle_action(argv)
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
        let mut argv = vec!["--repo".to_string(), main_root.display().to_string()];
        if thread_id.starts_with('-') {
            argv.extend([
                "merge".to_string(),
                "--preview".to_string(),
                "--".to_string(),
                thread_id.to_string(),
            ]);
        } else {
            argv.extend([
                "merge".to_string(),
                thread_id.to_string(),
                "--preview".to_string(),
            ]);
        }
        return heddle_action(argv);
    }
    if action == land_local_command(thread_id) {
        let mut argv = vec![
            "--repo".to_string(),
            main_root.display().to_string(),
            "land".to_string(),
        ];
        argv.extend(thread_flag_args(thread_id));
        argv.push("--no-push".to_string());
        return heddle_action(argv);
    }
    if action == land_push_command(thread_id) {
        let mut argv = vec![
            "--repo".to_string(),
            main_root.display().to_string(),
            "land".to_string(),
        ];
        argv.extend(thread_flag_args(thread_id));
        argv.push("--push".to_string());
        return heddle_action(argv);
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
            land_push_command(id),
            land_push_remote_command(id, "origin"),
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
        assert_eq!(land_local_command(id), "heddle land '--thread=-foo' --no-push");
        assert_eq!(land_push_command(id), "heddle land '--thread=-foo' --push");
        assert_eq!(
            land_push_remote_command(id, "origin"),
            "heddle land '--thread=-foo' --push --remote origin"
        );
        assert_eq!(merge_preview_command(id), "heddle merge --preview -- -foo");
        assert_eq!(switch_thread_command(id), "heddle switch -- -foo");
    }

    #[test]
    fn switch_thread_command_is_stable_and_copy_pasteable() {
        assert_eq!(switch_thread_command("feature/demo"), "heddle switch feature/demo");
        assert_eq!(
            switch_thread_command("feature with spaces"),
            "heddle switch 'feature with spaces'"
        );
    }
}
