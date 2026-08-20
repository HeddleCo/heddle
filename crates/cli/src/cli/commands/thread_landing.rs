// SPDX-License-Identifier: Apache-2.0
//! Shared commands for the ready -> land thread landing loop.

use std::path::Path;

use heddle_core::{recovery_scope_checkout, status::next_action::thread_flag_args};

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

/// Ready next-action for a checkout that may have been selected with `--repo`.
///
/// `--thread` stays omitted when that checkout's current thread is the one
/// being readied. `--repo` is kept only when the checkout was selected
/// explicitly and is not the process cwd (heddle#1473).
pub(crate) fn land_action_for_ready(
    repo: &repo::Repository,
    thread_id: &str,
    explicit_repo: Option<&Path>,
    cwd: &Path,
) -> String {
    land_action_for_selected_checkout(
        thread_id,
        checkout_is_thread(repo, thread_id),
        explicit_repo,
        cwd,
    )
}

fn land_action_for_selected_checkout(
    thread_id: &str,
    current_thread: bool,
    explicit_repo: Option<&Path>,
    cwd: &Path,
) -> String {
    let Some(path) = explicit_repo.filter(|path| recovery_scope_checkout(path, cwd).is_some())
    else {
        return land_action_when(thread_id, current_thread);
    };
    // `heddle_action` quotes through the shared single-quote helper.
    // Do not splice `--repo` via `scope_action_to_repo`: that double-quotes
    // and leaves `$` / backticks expandable if the next action is pasted.
    let mut argv = vec![
        "--repo".to_string(),
        path.display().to_string(),
        "land".to_string(),
    ];
    if !current_thread {
        argv.extend(thread_flag_args(thread_id));
    }
    heddle_action(argv)
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

    #[test]
    fn explicit_repo_keeps_repo_and_thread_only_when_needed() {
        use super::super::command_catalog::validate_recommended_action;

        let checkout = Path::new("/work/threads/feature");
        let other = Path::new("/work/main");

        let current_via_repo =
            land_action_for_selected_checkout("feature/demo", true, Some(checkout), other);
        assert_eq!(current_via_repo, "heddle --repo /work/threads/feature land");
        validate_recommended_action(&current_via_repo)
            .unwrap_or_else(|e| panic!("explicit-repo current-thread land must validate: {e}"));

        let named_via_repo =
            land_action_for_selected_checkout("feature/demo", false, Some(other), checkout);
        assert_eq!(
            named_via_repo,
            "heddle --repo /work/main land --thread feature/demo"
        );
        validate_recommended_action(&named_via_repo)
            .unwrap_or_else(|e| panic!("explicit-repo named-thread land must validate: {e}"));

        assert_eq!(
            land_action_for_selected_checkout("feature/demo", true, Some(checkout), checkout),
            "heddle land"
        );
        assert_eq!(
            land_action_for_selected_checkout("feature/demo", true, None, other),
            "heddle land"
        );
    }

    #[test]
    fn explicit_repo_single_quotes_shell_metacharacters() {
        use super::super::command_catalog::validate_recommended_action;

        let other = Path::new("/work/main");
        for (path, raw) in [
            (Path::new("/tmp/work-$(whoami)"), "/tmp/work-$(whoami)"),
            (Path::new("/tmp/work-`id`"), "/tmp/work-`id`"),
            (Path::new("/tmp/$HOME-checkout"), "/tmp/$HOME-checkout"),
        ] {
            let action = land_action_for_selected_checkout("feature/demo", true, Some(path), other);
            let expected = heddle_action(["--repo", raw, "land"]);
            assert_eq!(action, expected, "path {raw}");
            assert_eq!(
                action,
                format!("heddle --repo '{raw}' land"),
                "metacharacter --repo must be single-quoted: {action}"
            );
            assert!(
                !action.contains(&format!("\"{raw}\"")),
                "must not use expandable double quotes: {action}"
            );
            validate_recommended_action(&action)
                .unwrap_or_else(|e| panic!("quoted --repo land must validate for {raw}: {e}"));

            let named = land_action_for_selected_checkout("feature/demo", false, Some(path), other);
            assert_eq!(
                named,
                heddle_action(["--repo", raw, "land", "--thread", "feature/demo"]),
                "named-thread path {raw}"
            );
            validate_recommended_action(&named).unwrap_or_else(|e| {
                panic!("quoted --repo land --thread must validate for {raw}: {e}")
            });
        }

        let expandable = heddle_core::quote_recommended_action_arg("/tmp/work-$(whoami)");
        assert_eq!(
            expandable, "\"/tmp/work-$(whoami)\"",
            "the rejected helper still double-quotes `$()` — that is the P1"
        );
        assert_ne!(
            land_action_for_selected_checkout(
                "feature/demo",
                true,
                Some(Path::new("/tmp/work-$(whoami)")),
                other,
            ),
            format!("heddle --repo {expandable} land")
        );
    }
}
