// SPDX-License-Identifier: Apache-2.0
//! Tests for the machine-readable command catalog.
//!
//! Extracted verbatim from `command_catalog.rs` (heddle#609 phase 1):
//! the `mod tests` body moved into this sibling file unchanged (de-indented
//! one level). It referenced the parent via `super::*` inline and continues
//! to do so as a sibling module -- pure code movement, no logic change.
//!
//! Gated by the `#[cfg(test)] mod tests;` declaration in the parent module.

use std::collections::BTreeSet;

use clap::Parser;

use super::*;

struct RuntimeContractParseSample {
    path: &'static [&'static str],
    argv_tail: &'static [&'static str],
}

// Representative parseable invocations for every runtime leaf command in
// CONTRACTS. The only intentionally skipped rows are non-runtime grouping
// contracts whose Clap variants require a subcommand, e.g. `thread`,
// `bridge git`, and `redact trust`.
const RUNTIME_CONTRACT_PARSE_SAMPLES: &[RuntimeContractParseSample] = &[
    sample(&["abort"], &["abort"]),
    sample(&["adopt"], &["adopt"]),
    sample(&["actor", "spawn"], &["actor", "spawn"]),
    sample(&["actor", "list"], &["actor", "list"]),
    sample(&["actor", "show"], &["actor", "show"]),
    sample(&["actor", "explain"], &["actor", "explain"]),
    sample(&["actor", "done"], &["actor", "done"]),
    sample(&["agent", "serve"], &["agent", "serve"]),
    sample(&["agent", "status"], &["agent", "status"]),
    sample(&["agent", "stop"], &["agent", "stop"]),
    sample(
        &["agent", "reserve"],
        &["agent", "reserve", "--thread", "main"],
    ),
    sample(
        &["agent", "heartbeat"],
        &["agent", "heartbeat", "--session", "session-1"],
    ),
    sample(
        &["agent", "capture"],
        &["agent", "capture", "--session", "session-1"],
    ),
    sample(
        &["agent", "ready"],
        &["agent", "ready", "--session", "session-1"],
    ),
    sample(
        &["agent", "release"],
        &["agent", "release", "--session", "session-1"],
    ),
    sample(&["agent", "list"], &["agent", "list"]),
    sample(
        &["agent", "task", "create"],
        &[
            "agent",
            "task",
            "create",
            "--task-id",
            "task-1",
            "--title",
            "Task one",
            "--thread",
            "feature/task-1",
        ],
    ),
    sample(&["agent", "task", "list"], &["agent", "task", "list"]),
    sample(
        &["agent", "task", "show"],
        &["agent", "task", "show", "task-1"],
    ),
    sample(
        &["agent", "task", "update"],
        &["agent", "task", "update", "task-1", "--status", "complete"],
    ),
    sample(
        &["agent", "fanout", "plan"],
        &[
            "agent",
            "fanout",
            "plan",
            "--title",
            "Coordinate lanes",
            "--lane",
            "feature/a=../a:Implement A",
        ],
    ),
    sample(
        &["agent", "fanout", "start"],
        &[
            "agent",
            "fanout",
            "start",
            "--title",
            "Coordinate lanes",
            "--lane",
            "feature/a=../a:Implement A",
        ],
    ),
    #[cfg(feature = "client")]
    sample(&["auth", "login"], &["auth", "login", "--no-browser"]),
    #[cfg(feature = "client")]
    sample(&["auth", "logout"], &["auth", "logout"]),
    #[cfg(feature = "client")]
    sample(&["auth", "status"], &["auth", "status"]),
    #[cfg(feature = "client")]
    sample(
        &["auth", "create-service-token"],
        &[
            "auth",
            "create-service-token",
            "github-ci-main",
            "--namespace",
            "heddle/platform",
        ],
    ),
    #[cfg(feature = "git-overlay")]
    sample(&["import", "git"], &["import", "git"]),
    #[cfg(feature = "git-overlay")]
    sample(
        &["export", "git"],
        &["export", "git", "--destination", "out.git"],
    ),
    #[cfg(feature = "git-overlay")]
    sample(&["bridge", "git", "status"], &["bridge", "git", "status"]),
    #[cfg(feature = "git-overlay")]
    sample(&["bridge", "git", "export"], &["bridge", "git", "export"]),
    #[cfg(feature = "git-overlay")]
    sample(&["bridge", "git", "import"], &["bridge", "git", "import"]),
    #[cfg(feature = "git-overlay")]
    sample(&["bridge", "git", "sync"], &["bridge", "git", "sync"]),
    #[cfg(feature = "git-overlay")]
    sample(
        &["bridge", "git", "reconcile"],
        &[
            "bridge",
            "git",
            "reconcile",
            "--prefer",
            "heddle",
            "--ref",
            "main",
        ],
    ),
    #[cfg(feature = "git-overlay")]
    sample(&["bridge", "git", "push"], &["bridge", "git", "push"]),
    #[cfg(feature = "git-overlay")]
    sample(&["bridge", "git", "pull"], &["bridge", "git", "pull"]),
    #[cfg(all(feature = "git-overlay", feature = "ingest"))]
    sample(
        &["bridge", "git", "reason"],
        &["bridge", "git", "reason", "--path", "."],
    ),
    sample(&["capture"], &["capture"]),
    sample(&["checkpoint"], &["checkpoint"]),
    sample(&["cherry-pick"], &["cherry-pick", "abc123"]),
    sample(&["clean"], &["clean"]),
    sample(&["clone"], &["clone", "remote", "local"]),
    sample(
        &["collapse"],
        &["collapse", "s1", "s2", "--into", "squashed"],
    ),
    sample(&["commit"], &["commit"]),
    sample(&["continue"], &["continue"]),
    sample(&["expand"], &["expand", "HEAD"]),
    sample(
        &["context", "set"],
        &["context", "set", "--path", "src/lib.rs"],
    ),
    sample(
        &["context", "get"],
        &["context", "get", "--path", "src/lib.rs"],
    ),
    sample(&["context", "list"], &["context", "list"]),
    sample(&["context", "history"], &["context", "history", "ctx-1"]),
    sample(&["context", "edit"], &["context", "edit", "ctx-1"]),
    sample(
        &["context", "supersede"],
        &["context", "supersede", "ctx-1", "--path", "src/lib.rs"],
    ),
    sample(
        &["context", "rm"],
        &["context", "rm", "--path", "src/lib.rs"],
    ),
    sample(&["context", "check"], &["context", "check"]),
    sample(&["context", "suggest"], &["context", "suggest"]),
    sample(&["context", "audit"], &["context", "audit"]),
    sample(&["daemon", "serve"], &["daemon", "serve"]),
    sample(&["daemon", "status"], &["daemon", "status"]),
    sample(&["daemon", "stop"], &["daemon", "stop"]),
    sample(&["diff"], &["diff"]),
    sample(
        &["discuss", "open"],
        &["discuss", "open", "src/lib.rs", "symbol", "body"],
    ),
    sample(
        &["discuss", "append"],
        &["discuss", "append", "discussion-1", "body"],
    ),
    sample(
        &["discuss", "resolve"],
        &["discuss", "resolve", "discussion-1", "--mode", "dismiss"],
    ),
    sample(&["discuss", "list"], &["discuss", "list"]),
    sample(&["discuss", "show"], &["discuss", "show", "discussion-1"]),
    sample(&["doctor"], &["doctor"]),
    sample(&["doctor", "docs"], &["doctor", "docs"]),
    sample(&["doctor", "schemas"], &["doctor", "schemas"]),
    sample(&["fetch"], &["fetch"]),
    sample(&["fsck"], &["fsck"]),
    sample(&["oplog", "recover"], &["oplog", "recover"]),
    #[cfg(feature = "git-overlay")]
    sample(&["git-overlay"], &["git-overlay"]),
    sample(&["help"], &["help"]),
    sample(&["hook", "list"], &["hook", "list"]),
    sample(&["hook", "install"], &["hook", "install", "pre-snapshot"]),
    sample(
        &["hook", "uninstall"],
        &["hook", "uninstall", "pre-snapshot"],
    ),
    sample(&["hook", "events"], &["hook", "events"]),
    sample(&["init"], &["init"]),
    sample(&["integration", "list"], &["integration", "list"]),
    sample(&["integration", "install"], &["integration", "install"]),
    sample(&["integration", "doctor"], &["integration", "doctor"]),
    sample(&["integration", "uninstall"], &["integration", "uninstall"]),
    sample(&["integration", "upgrade"], &["integration", "upgrade"]),
    sample(
        &["integration", "relay"],
        &["integration", "relay", "codex", "agent_done"],
    ),
    sample(&["log"], &["log"]),
    sample(&["maintenance", "inspect"], &["maintenance", "inspect"]),
    sample(&["maintenance", "run"], &["maintenance", "run"]),
    sample(&["maintenance", "gc"], &["maintenance", "gc"]),
    sample(&["maintenance", "index"], &["maintenance", "index"]),
    sample(&["maintenance", "monitor"], &["maintenance", "monitor"]),
    sample(&["merge"], &["merge", "feature"]),
    #[cfg(feature = "client")]
    sample(
        &["presence", "publish"],
        &["presence", "publish", "--session", "session-1"],
    ),
    sample(&["pull"], &["pull"]),
    sample(&["push"], &["push"]),
    sample(&["query"], &["query"]),
    sample(&["ready"], &["ready"]),
    sample(&["rebase"], &["rebase"]),
    sample(
        &["redact", "apply"],
        &[
            "redact",
            "apply",
            "HEAD",
            "--path",
            "secret.txt",
            "--reason",
            "secret",
        ],
    ),
    sample(&["redact", "list"], &["redact", "list"]),
    sample(&["redact", "show"], &["redact", "show", "redaction-1"]),
    sample(
        &["redact", "purge", "apply"],
        &[
            "redact",
            "purge",
            "apply",
            "HEAD",
            "--path",
            "secret.txt",
            "--force",
        ],
    ),
    sample(&["redact", "purge", "list"], &["redact", "purge", "list"]),
    sample(
        &["redact", "trust", "add"],
        &[
            "redact",
            "trust",
            "add",
            "--algorithm",
            "ed25519",
            "--public-key",
            "abcd",
        ],
    ),
    sample(&["redact", "trust", "list"], &["redact", "trust", "list"]),
    sample(
        &["redact", "trust", "remove"],
        &["redact", "trust", "remove", "abcd"],
    ),
    sample(&["remote", "list"], &["remote", "list"]),
    sample(
        &["remote", "add"],
        &["remote", "add", "origin", "localhost:9418"],
    ),
    sample(&["remote", "remove"], &["remote", "remove", "origin"]),
    sample(
        &["remote", "set-default"],
        &["remote", "set-default", "origin"],
    ),
    sample(&["remote", "show"], &["remote", "show", "origin"]),
    sample(&["resolve"], &["resolve"]),
    sample(&["retro"], &["retro"]),
    sample(&["revert"], &["revert", "HEAD"]),
    sample(&["review", "show"], &["review", "show"]),
    sample(
        &["review", "sign"],
        &[
            "review",
            "sign",
            "HEAD",
            "--kind",
            "read",
            "--public-key",
            "abcd",
            "--signature",
            "ef01",
            "--signed-at-unix",
            "1",
        ],
    ),
    sample(&["review", "next"], &["review", "next"]),
    sample(&["review", "health"], &["review", "health"]),
    sample(&["run"], &["run", "true"]),
    sample(&["schemas"], &["schemas"]),
    #[cfg(feature = "semantic")]
    sample(&["semantic", "hot"], &["semantic", "hot"]),
    sample(
        &["session", "start"],
        &[
            "session",
            "start",
            "--provider",
            "openai",
            "--model",
            "gpt-5",
        ],
    ),
    sample(
        &["session", "segment"],
        &[
            "session",
            "segment",
            "--provider",
            "openai",
            "--model",
            "gpt-5",
        ],
    ),
    sample(&["session", "end"], &["session", "end"]),
    sample(&["session", "show"], &["session", "show"]),
    sample(&["session", "list"], &["session", "list"]),
    sample(&["shell", "init"], &["shell", "init", "bash"]),
    sample(&["shell", "completion"], &["shell", "completion", "bash"]),
    sample(&["shell", "prompt"], &["shell", "prompt"]),
    sample(&["complete"], &["__complete", "threads"]),
    sample(&["land"], &["land"]),
    sample(&["show"], &["show", "HEAD"]),
    sample(&["start"], &["start", "feature"]),
    sample(&["stash", "push"], &["stash", "push"]),
    sample(&["stash", "list"], &["stash", "list"]),
    sample(&["stash", "pop"], &["stash", "pop"]),
    sample(&["stash", "apply"], &["stash", "apply"]),
    sample(&["stash", "drop"], &["stash", "drop"]),
    sample(&["stash", "clear"], &["stash", "clear"]),
    sample(&["stash", "show"], &["stash", "show"]),
    sample(&["status"], &["status"]),
    #[cfg(feature = "client")]
    sample(
        &["support", "grant"],
        &[
            "support",
            "grant",
            "support@heddle.dev",
            "--namespace",
            "heddle/platform",
            "--reason",
            "release verification",
        ],
    ),
    #[cfg(feature = "client")]
    sample(
        &["support", "list"],
        &["support", "list", "--namespace", "heddle/platform"],
    ),
    #[cfg(feature = "client")]
    sample(
        &["support", "revoke"],
        &["support", "revoke", "00000000-0000-0000-0000-000000000000"],
    ),
    #[cfg(feature = "client")]
    sample(
        &["spool", "attach"],
        &["spool", "attach", "acme/root", "acme/lib"],
    ),
    #[cfg(feature = "client")]
    sample(
        &["spool", "detach"],
        &["spool", "detach", "acme/root", "libs"],
    ),
    #[cfg(feature = "client")]
    sample(&["spool", "children"], &["spool", "children", "acme/root"]),
    #[cfg(feature = "client")]
    sample(
        &["spool", "governance"],
        &["spool", "governance", "acme/root"],
    ),
    #[cfg(feature = "client")]
    sample(
        &["spool", "membership"],
        &["spool", "membership", "acme/root"],
    ),
    sample(&["switch"], &["switch", "main"]),
    sample(&["sync"], &["sync"]),
    sample(&["thread", "create"], &["thread", "create", "feature"]),
    sample(&["thread", "current"], &["thread", "current"]),
    sample(&["thread", "switch"], &["thread", "switch", "feature"]),
    sample(&["thread", "cd"], &["thread", "cd", "feature"]),
    sample(&["thread", "list"], &["thread", "list"]),
    sample(&["thread", "show"], &["thread", "show"]),
    sample(&["thread", "captures"], &["thread", "captures"]),
    sample(
        &["thread", "rename"],
        &["thread", "rename", "old-feature", "new-feature"],
    ),
    sample(&["thread", "refresh"], &["thread", "refresh", "feature"]),
    sample(
        &["thread", "move"],
        &["thread", "move", "source", "dest", "--path", "src/lib.rs"],
    ),
    sample(&["thread", "absorb"], &["thread", "absorb", "feature"]),
    sample(&["thread", "resolve"], &["thread", "resolve", "feature"]),
    sample(&["thread", "promote"], &["thread", "promote", "feature"]),
    sample(&["thread", "drop"], &["thread", "drop", "feature"]),
    sample(
        &["thread", "approve"],
        &["thread", "approve", "source", "target"],
    ),
    sample(
        &["thread", "approvals"],
        &["thread", "approvals", "source", "target"],
    ),
    sample(
        &["thread", "revoke-approval"],
        &[
            "thread",
            "revoke-approval",
            "00000000-0000-0000-0000-000000000000",
        ],
    ),
    sample(
        &["thread", "check-merge"],
        &["thread", "check-merge", "source", "target"],
    ),
    sample(&["thread", "cleanup"], &["thread", "cleanup", "--merged"]),
    sample(&["thread", "marker", "list"], &["thread", "marker", "list"]),
    sample(
        &["thread", "marker", "create"],
        &["thread", "marker", "create", "checkpoint"],
    ),
    sample(
        &["thread", "marker", "delete"],
        &["thread", "marker", "delete", "checkpoint"],
    ),
    sample(
        &["thread", "marker", "show"],
        &["thread", "marker", "show", "checkpoint"],
    ),
    sample(&["timeline", "status"], &["timeline", "status"]),
    sample(
        &["timeline", "record-start"],
        &["timeline", "record-start", "--tool-call", "call-1"],
    ),
    sample(
        &["timeline", "record-finish"],
        &["timeline", "record-finish", "--tool-call", "call-1"],
    ),
    sample(
        &["timeline", "fork"],
        &["timeline", "fork", "--step", "tls-abc"],
    ),
    sample(
        &["timeline", "reset"],
        &["timeline", "reset", "--step", "tls-abc"],
    ),
    sample(&["timeline", "recover"], &["timeline", "recover"]),
    sample(&["transaction", "begin"], &["transaction", "begin"]),
    sample(
        &["transaction", "commit"],
        &["transaction", "commit", "tx-1"],
    ),
    sample(&["transaction", "abort"], &["transaction", "abort", "tx-1"]),
    sample(
        &["transaction", "status"],
        &["transaction", "status", "tx-1"],
    ),
    sample(&["verify"], &["verify"]),
    sample(
        &["visibility", "set"],
        &["visibility", "set", "HEAD", "--tier", "internal"],
    ),
    sample(
        &["visibility", "promote"],
        &["visibility", "promote", "HEAD", "--tier", "internal"],
    ),
    sample(&["visibility", "show"], &["visibility", "show", "HEAD"]),
    sample(&["visibility", "list"], &["visibility", "list"]),
    sample(&["try"], &["try", "true"]),
    sample(&["undo"], &["undo"]),
    sample(&["watch"], &["watch"]),
];

const fn sample(
    path: &'static [&'static str],
    argv_tail: &'static [&'static str],
) -> RuntimeContractParseSample {
    RuntimeContractParseSample { path, argv_tail }
}

#[test]
fn recommended_actions_parse_through_clap_or_registered_placeholders() {
    for action in [
        "",
        "heddle init",
        "heddle capture -m \"...\"",
        "heddle commit -m \"...\"",
        "heddle stash push -m \"...\"",
        "heddle capture -m \"Preserve raw Git operation work\"",
        "heddle switch <branch>",
        "heddle start feature/auth --path <dir>",
        "heddle clone <remote> <fresh-path>",
        "heddle clone <local-path> <path>",
        "heddle clone /tmp/source <path> --thread main",
        "heddle bridge git import --path <full-git-repo> --ref <ref>",
        "heddle thread promote main",
        "heddle thread resolve main",
    ] {
        validate_recommended_action(action)
            .unwrap_or_else(|err| panic!("expected `{action}` to validate: {err}"));
    }
    #[cfg(feature = "git-overlay")]
    {
        for action in [
            "heddle bridge git import --ref main",
            "heddle bridge git import --ref origin/main",
            "heddle merge origin/main --preview",
            "heddle bridge git reconcile --ref main --preview",
            "heddle bridge git reconcile --prefer heddle --ref main --preview",
        ] {
            validate_recommended_action(action)
                .unwrap_or_else(|err| panic!("expected `{action}` to validate: {err}"));
        }
    }
}

#[test]
fn recommended_action_templates_describe_display_only_placeholders() {
    let catalog = build_command_catalog();
    for placeholder in RECOMMENDED_ACTION_PLACEHOLDERS {
        assert!(
            recommended_action_template(placeholder).is_some(),
            "placeholder `{placeholder}` must have a structured template"
        );
    }
    for template in &catalog.recommended_action_templates {
        validate_recommended_action(&template.action).unwrap_or_else(|err| {
            panic!(
                "recommended action template `{}` must validate: {err}",
                template.action
            )
        });
    }

    let commit = catalog
        .recommended_action_templates
        .iter()
        .find(|template| template.action == "heddle commit -m \"...\"")
        .expect("commit placeholder should have a structured template");
    assert_eq!(
        commit.argv_template,
        vec!["heddle", "commit", "-m", "<message>"]
    );
    assert_eq!(commit.required_inputs, vec!["message"]);
    assert!(commit.agent_may_fill);

    let template = recommended_action_template("heddle checkpoint -m \"...\"")
        .expect("checkpoint placeholder should resolve");
    assert_eq!(
        template.argv_template,
        vec!["heddle", "checkpoint", "-m", "<message>"]
    );

    let switch = recommended_action_template("heddle switch <branch>")
        .expect("switch placeholder should resolve");
    assert_eq!(switch.argv_template, vec!["heddle", "switch", "<branch>"]);
    assert_eq!(switch.required_inputs, vec!["branch"]);
    assert!(!switch.agent_may_fill);

    let clone = recommended_action_template("heddle clone <remote> <fresh-path>")
        .expect("clone recovery placeholder should resolve");
    assert_eq!(
        clone.argv_template,
        vec!["heddle", "clone", "<remote>", "<fresh-path>"]
    );
    assert_eq!(clone.required_inputs, vec!["remote", "path"]);
    assert!(!clone.agent_may_fill);

    let start = recommended_action_template("heddle start feature/auth --path <dir>")
        .expect("start path placeholder should resolve");
    assert_eq!(
        start.argv_template,
        vec!["heddle", "start", "feature/auth", "--path", "<dir>"]
    );
    assert_eq!(start.required_inputs, vec!["dir"]);
    assert!(start.agent_may_fill);

    let local_clone = recommended_action_template("heddle clone <local-path> <path>")
        .expect("local clone recovery placeholder should resolve");
    assert_eq!(
        local_clone.argv_template,
        vec!["heddle", "clone", "<local-path>", "<path>"]
    );
    assert_eq!(local_clone.required_inputs, vec!["local_path", "path"]);
    assert!(!local_clone.agent_may_fill);

    let dynamic_clone =
        recommended_action_template("heddle clone /tmp/source <path> --thread main")
            .expect("dynamic clone recovery placeholder should resolve");
    assert_eq!(
        dynamic_clone.argv_template,
        vec![
            "heddle",
            "clone",
            "/tmp/source",
            "<path>",
            "--thread",
            "main"
        ]
    );
    assert_eq!(dynamic_clone.required_inputs, vec!["path"]);
    assert!(!dynamic_clone.agent_may_fill);

    let import =
        recommended_action_template("heddle bridge git import --path <full-git-repo> --ref <ref>")
            .expect("shallow import recovery placeholder should resolve");
    assert_eq!(
        import.argv_template,
        vec![
            "heddle",
            "bridge",
            "git",
            "import",
            "--path",
            "<full-git-repo>",
            "--ref",
            "<ref>"
        ]
    );
    assert_eq!(import.required_inputs, vec!["path", "ref"]);
    assert!(!import.agent_may_fill);

    let merge = recommended_action_template("heddle merge <thread> --git-commit")
        .expect("merge recovery placeholder should resolve");
    assert_eq!(
        merge.argv_template,
        vec!["heddle", "merge", "<thread>", "--git-commit"]
    );
    assert_eq!(merge.required_inputs, vec!["thread"]);
    assert!(!merge.agent_may_fill);
}

#[test]
fn action_fields_template_dirty_worktree_message_placeholders() {
    for (action, expected_argv_template) in [
        (
            "heddle commit -m \"...\"",
            vec!["heddle", "commit", "-m", "<message>"],
        ),
        (
            "heddle capture -m \"...\"",
            vec!["heddle", "capture", "-m", "<message>"],
        ),
        (
            "heddle stash push -m \"...\"",
            vec!["heddle", "stash", "push", "-m", "<message>"],
        ),
    ] {
        let fields = ActionFields::from_action(action);
        assert_eq!(fields.action.as_deref(), Some(action));
        let template = fields
            .template
            .unwrap_or_else(|| panic!("`{action}` should expose a structured template"));
        assert_eq!(template.argv_template, expected_argv_template);
        assert_eq!(template.required_inputs, vec!["message"]);
        assert!(template.agent_may_fill);
    }
}

#[test]
fn action_fields_template_argv_normalized_message_placeholders() {
    for (action, expected_argv_template) in [
        (
            "heddle commit -m ...",
            vec!["heddle", "commit", "-m", "<message>"],
        ),
        (
            "heddle capture -m ...",
            vec!["heddle", "capture", "-m", "<message>"],
        ),
        (
            "heddle stash push -m ...",
            vec!["heddle", "stash", "push", "-m", "<message>"],
        ),
    ] {
        let fields = ActionFields::from_action(action);
        assert_eq!(fields.action.as_deref(), Some(action));
        let template = fields
            .template
            .unwrap_or_else(|| panic!("`{action}` should expose a structured template"));
        assert_eq!(template.argv_template, expected_argv_template);
        assert_eq!(template.required_inputs, vec!["message"]);
        assert!(template.agent_may_fill);
    }
}

#[test]
fn display_only_recommended_actions_must_be_templated() {
    let err = validate_recommended_action("heddle switch <missing-template>")
        .expect_err("unregistered display placeholder should fail validation");
    assert!(
        err.contains("structured template"),
        "error should explain missing template: {err}"
    );

    assert!(
        recommended_action_template("heddle switch <missing-template>").is_none(),
        "unregistered display placeholder must not resolve to a fillable template"
    );
}

#[test]
fn recommended_action_validator_rejects_unknown_commands() {
    let err = validate_recommended_action("heddle definitely-not-a-command")
        .expect_err("unknown heddle command should fail validation");
    assert!(
        err.contains("definitely-not-a-command"),
        "error should name the bad command: {err}"
    );

    let err = validate_recommended_action("git status")
        .expect_err("raw git action must be explicitly registered");
    assert!(
        err.contains("registered as a placeholder"),
        "error should explain placeholder registration: {err}"
    );
}

#[test]
fn leading_dash_thread_breadcrumbs_pass_validation() {
    // A historical / `new_unchecked` thread id literally named `-foo` renders
    // breadcrumbs via the `=` (flag) and `--` (positional) forms; the
    // validator splits to argv and runs clap, which would reject the bare
    // `--thread -foo` form as an unknown flag. (heddle#464 round 8.)
    for action in [
        repo::RecommendedAction::Sync,
        repo::RecommendedAction::Ready,
        repo::RecommendedAction::Land,
        repo::RecommendedAction::Promote,
    ] {
        if let Some(cmd) = action.command("-foo") {
            validate_recommended_action(&cmd).unwrap_or_else(|err| {
                panic!("breadcrumb `{cmd}` must validate for a leading-dash id: {err}")
            });
        }
    }
}

#[test]
fn action_fields_fail_loudly_for_invalid_recommendations() {
    let panic = std::panic::catch_unwind(|| ActionFields::from_action("git status"));
    assert!(
        panic.is_err(),
        "ActionFields must not silently erase invalid action sidecars"
    );
}

#[test]
fn recommended_action_parser_supports_shell_quoted_arguments() {
    let template = recommended_action_template("heddle merge 'feature with spaces' --preview")
        .expect("single-quoted thread action should resolve to a template");
    assert_eq!(
        template.argv_template[1..],
        ["merge", "feature with spaces", "--preview"]
    );

    let template = recommended_action_template("heddle merge 'feature '\\''quoted'\\''' --preview")
        .expect("shell-quoted apostrophe should resolve to a template");
    assert_eq!(
        template.argv_template[1..],
        ["merge", "feature 'quoted'", "--preview"]
    );
}

#[test]
fn checked_action_builder_quotes_and_validates_from_argv() {
    let action = heddle_action(["merge", "feature with spaces", "--preview"]);
    assert_eq!(action, "heddle merge 'feature with spaces' --preview");
    let template =
        recommended_action_template(&action).expect("built action should resolve to a template");
    assert_eq!(
        template.argv_template[1..],
        ["merge", "feature with spaces", "--preview"]
    );

    let panic = std::panic::catch_unwind(|| checked_action_from_argv(["git", "status"]));
    assert!(
        panic.is_err(),
        "non-Heddle actions should not enter runtime advice sidecars"
    );
}

#[test]
fn command_contract_table_matches_clap_command_tree() {
    let raw_contract_paths = CONTRACTS
        .iter()
        .map(|entry| {
            entry
                .path
                .iter()
                .map(|part| (*part).to_string())
                .collect::<Vec<_>>()
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        raw_contract_paths.len(),
        CONTRACTS.len(),
        "command contract table contains duplicate paths"
    );
    let active_contract_paths = active_command_contract_entries()
        .iter()
        .copied()
        .map(|entry| {
            entry
                .path
                .iter()
                .map(|part| (*part).to_string())
                .collect::<Vec<_>>()
        })
        .collect::<BTreeSet<_>>();

    let mut clap_paths = BTreeSet::new();
    collect_clap_command_paths(&Cli::command(), &mut Vec::new(), &mut clap_paths);

    let missing_contracts = clap_paths
        .difference(&active_contract_paths)
        .map(|path| path.join(" "))
        .collect::<Vec<_>>();
    assert!(
        missing_contracts.is_empty(),
        "Clap exposes command path(s) with no command contract: {missing_contracts:?}"
    );

    let stale_contracts = active_contract_paths
        .difference(&clap_paths)
        .map(|path| path.join(" "))
        .collect::<Vec<_>>();
    assert!(
        stale_contracts.is_empty(),
        "command contract table contains path(s) not exposed by Clap: {stale_contracts:?}"
    );
}

fn collect_clap_command_paths(
    command: &clap::Command,
    prefix: &mut Vec<String>,
    out: &mut BTreeSet<Vec<String>>,
) {
    for subcommand in command.get_subcommands() {
        prefix.push(subcommand.get_name().to_string());
        out.insert(prefix.clone());
        collect_clap_command_paths(subcommand, prefix, out);
        prefix.pop();
    }
}

#[test]
fn parsed_runtime_contract_lookup_matches_contract_table_for_parseable_commands() {
    let active_contracts = active_command_contract_entries();
    let sample_paths = RUNTIME_CONTRACT_PARSE_SAMPLES
        .iter()
        .map(|sample| sample.path.to_vec())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        sample_paths.len(),
        RUNTIME_CONTRACT_PARSE_SAMPLES.len(),
        "runtime contract parse samples contain duplicate paths"
    );

    let child_contract_paths = contract_paths_with_children(active_contracts);
    let unsampled_contracts = active_contracts
        .iter()
        .filter(|entry| !child_contract_paths.contains(entry.path))
        .filter(|entry| !sample_paths.contains(entry.path))
        .map(|entry| entry.path.join(" "))
        .collect::<Vec<_>>();
    assert!(
        unsampled_contracts.is_empty(),
        "parseable leaf/runtime contract path(s) need parse samples: {unsampled_contracts:?}"
    );

    for sample in RUNTIME_CONTRACT_PARSE_SAMPLES {
        let expected = active_contracts
            .iter()
            .find(|entry| entry.path == sample.path)
            .unwrap_or_else(|| {
                panic!(
                    "runtime contract parse sample references missing contract `{}`",
                    sample.path.join(" ")
                )
            });
        let mut argv = vec!["heddle"];
        argv.extend_from_slice(sample.argv_tail);
        let cli = Cli::try_parse_from(argv.clone())
            .unwrap_or_else(|err| panic!("failed to parse sample {argv:?}: {err}"));
        let runtime = command_runtime_contract_for_command(&cli.command);

        assert_eq!(
            runtime.path,
            expected.path,
            "parsed sample {argv:?} must resolve to contract path `{}`",
            expected.path.join(" ")
        );
        assert_eq!(runtime.display, expected.path.join(" "));
        assert_eq!(runtime.display, command_path(&cli.command).join(" "));
    }
}

#[test]
fn json_compact_runtime_contract_is_projection_or_rejection() {
    let expected_compact = BTreeSet::from([
        "abort".to_string(),
        "capture".to_string(),
        "continue".to_string(),
        "land".to_string(),
        "merge".to_string(),
        "ready".to_string(),
        "status".to_string(),
        "sync".to_string(),
    ]);
    let actual_compact = active_command_contract_entries()
        .iter()
        .filter(|entry| entry.contract.supports_json_compact)
        .map(|entry| entry.path.join(" "))
        .collect::<BTreeSet<_>>();
    assert_eq!(actual_compact, expected_compact);

    let json_output_commands = active_command_contract_entries()
        .iter()
        .filter(|entry| entry.contract.supports_json)
        .map(|entry| entry.path.join(" "))
        .collect::<BTreeSet<_>>();
    let compact_rejections = active_command_contract_entries()
        .iter()
        .filter(|entry| entry.contract.supports_json && !entry.contract.supports_json_compact)
        .map(|entry| entry.path.join(" "))
        .collect::<BTreeSet<_>>();
    let classified_commands = actual_compact
        .union(&compact_rejections)
        .cloned()
        .collect::<BTreeSet<_>>();
    assert_eq!(
        classified_commands, json_output_commands,
        "every JSON-output command must either project json-compact or reject it before execution"
    );
    assert!(
        compact_rejections.contains("query"),
        "the harness must include commands that accept --output json but reject json-compact"
    );

    for sample in RUNTIME_CONTRACT_PARSE_SAMPLES {
        let mut argv = vec!["heddle", "--output", "json-compact"];
        argv.extend_from_slice(sample.argv_tail);
        let cli = Cli::try_parse_from(argv.clone())
            .unwrap_or_else(|err| panic!("failed to parse sample {argv:?}: {err}"));
        let runtime = command_runtime_contract_for_command(&cli.command);
        assert!(
            runtime.supports_json_compact || !expected_compact.contains(&runtime.display),
            "`{}` accepts json-compact at parse time but lacks an explicit compact projection; main must reject it before command execution",
            runtime.display
        );
        assert!(
            runtime.supports_json || !runtime.supports_json_compact,
            "`{}` cannot support json-compact without supporting json",
            runtime.display
        );
    }
}

fn contract_paths_with_children(
    entries: &[&'static CommandContractEntry],
) -> BTreeSet<Vec<&'static str>> {
    entries
        .iter()
        .filter(|candidate| {
            entries.iter().any(|entry| {
                entry.path.len() > candidate.path.len() && entry.path.starts_with(candidate.path)
            })
        })
        .map(|entry| entry.path.to_vec())
        .collect()
}

#[test]
fn command_contract_metadata_is_internally_consistent() {
    for entry in CONTRACTS {
        let display = entry.path.join(" ");
        let contract = entry.contract;
        let json_capable = matches!(contract.json_kind, "json" | "jsonl" | "json_or_jsonl");
        assert_eq!(
            contract.supports_json, json_capable,
            "`{display}` supports_json must agree with json_kind `{}`",
            contract.json_kind
        );
        assert!(
            matches!(
                contract.json_kind,
                "json" | "jsonl" | "json_or_jsonl" | "none"
            ),
            "`{display}` has unknown json_kind `{}`",
            contract.json_kind
        );
        assert!(
            matches!(
                contract.surface,
                "native" | "git_adapter" | "automation" | "admin" | "internal"
            ),
            "`{display}` has unknown product surface `{}`",
            contract.surface
        );
        assert!(
            matches!(
                contract.help_visibility,
                "everyday" | "advanced" | "git_adapter" | "hidden"
            ),
            "`{display}` has unknown help visibility `{}`",
            contract.help_visibility
        );
        if contract.help_visibility == "git_adapter" {
            assert_eq!(
                contract.surface, "git_adapter",
                "`{display}` Git adapter commands must live on the Git adapter surface"
            );
            assert!(
                contract.canonical_command.is_some(),
                "`{display}` Git-shaped aliases must name a canonical Heddle command"
            );
            assert!(
                contract.canonical_kind.is_some(),
                "`{display}` Git-shaped aliases must classify the canonical action"
            );
            assert!(
                contract.canonical_note.is_some(),
                "`{display}` Git-shaped aliases must explain the canonical action"
            );
        }
        if contract.help_visibility == "everyday" {
            assert!(
                contract.help_rank < 1000,
                "`{display}` everyday commands must choose an explicit help rank"
            );
        }
        if let Some(canonical) = contract.canonical_command {
            let canonical_kind = contract
                .canonical_kind
                .unwrap_or_else(|| panic!("`{display}` canonical command must have a kind"));
            assert!(
                matches!(
                    canonical_kind,
                    "direct_command" | "command_family" | "workflow" | "conceptual_home"
                ),
                "`{display}` has unknown canonical action kind `{canonical_kind}`"
            );
            assert!(
                raw_command_contract_for_path(canonical.split_whitespace()).is_some(),
                "`{display}` points at missing canonical command `{canonical}`"
            );
        } else {
            assert!(
                contract.canonical_kind.is_none() && contract.canonical_note.is_none(),
                "`{display}` cannot describe a canonical action without a canonical command"
            );
        }
        if contract.persists_op_id {
            assert!(
                contract.supports_op_id,
                "`{display}` cannot persist op-ids unless it supports op-id replay"
            );
            assert!(
                contract.mutates,
                "`{display}` cannot persist op-ids for an observe-only command"
            );
        }
        if contract.observe_only {
            assert!(
                !contract.mutates,
                "`{display}` cannot be both observe_only and mutating"
            );
            assert!(
                !contract.supports_op_id && !contract.persists_op_id,
                "`{display}` observe-only commands must not reserve op-id slots"
            );
            assert!(
                !contract.may_initialize
                    && !contract.may_import_git
                    && !contract.may_write_worktree
                    && !contract.may_move_ref
                    && !contract.destructive_requires_force
                    && !contract.writes_heddle_refs
                    && !contract.writes_git_refs
                    && !contract.writes_worktree
                    && !contract.writes_config
                    && !contract.writes_hooks
                    && !contract.network_io
                    && !contract.daemon_process
                    && !contract.object_gc
                    && !contract.external_command
                    && !contract.requires_git_executable
                    && !contract.destructive_data,
                "`{display}` observe-only commands must not advertise write side effects"
            );
        }
        assert!(
            !contract.requires_git_executable,
            "`{display}` must not require a `git` executable; Git-format work belongs in native/library code"
        );
        let effects = side_effects(contract);
        assert!(
            !effects.is_empty(),
            "`{display}` must advertise at least one side effect"
        );
        if contract.observe_only {
            assert_eq!(
                effects,
                vec!["observe_only"],
                "`{display}` observe-only side_effects must stay exact"
            );
        } else {
            for (flag, effect) in [
                (contract.may_initialize, "initialize"),
                (contract.may_import_git, "import_git"),
                (contract.writes_heddle_refs, "writes_heddle_refs"),
                (contract.writes_git_refs, "writes_git_refs"),
                (contract.writes_worktree, "writes_worktree"),
                (contract.writes_config, "writes_config"),
                (contract.writes_hooks, "writes_hooks"),
                (contract.network_io, "network_io"),
                (contract.daemon_process, "daemon_process"),
                (contract.object_gc, "object_gc"),
                (contract.external_command, "external_command"),
                (
                    contract.destructive_requires_force,
                    "destructive_requires_force",
                ),
                (contract.destructive_data, "destructive_data"),
            ] {
                assert_eq!(
                    effects.contains(&effect),
                    flag,
                    "`{display}` side_effects must mirror `{effect}`"
                );
            }
            if contract.may_write_worktree && !contract.writes_worktree {
                assert!(
                    effects.contains(&"may_write_worktree"),
                    "`{display}` side_effects must preserve flag-sensitive worktree writes"
                );
            }
        }
        assert_eq!(
            contract.may_move_ref,
            contract.writes_heddle_refs || contract.writes_git_refs,
            "`{display}` may_move_ref must summarize concrete ref dimensions"
        );
        if contract.destructive_requires_force {
            assert!(
                contract.mutates,
                "`{display}` destructive commands must be mutating commands"
            );
        }
        let schema_verbs = contract_schema_verbs(contract).collect::<Vec<_>>();
        let documented_schema_verbs =
            contract_documented_schema_verbs(contract).collect::<Vec<_>>();
        for verb in &documented_schema_verbs {
            assert!(
                schema_verbs.contains(verb),
                "`{display}` documents schema verb `{verb}` without registering it"
            );
        }
        for verb in contract.opaque_schema_verbs {
            assert!(
                schema_verbs.contains(verb),
                "`{display}` marks schema verb `{verb}` opaque without registering it"
            );
            assert!(
                documented_schema_verbs.contains(verb),
                "`{display}` marks schema verb `{verb}` opaque without documenting it"
            );
        }
        if !schema_verbs.is_empty() {
            assert!(
                contract.supports_json,
                "`{display}` registers JSON schema verbs but does not support JSON"
            );
        }
    }
}

#[cfg(not(feature = "git-overlay"))]
#[test]
fn native_only_catalog_excludes_git_overlay_commands() {
    let catalog = build_command_catalog();
    for display in [
        "bridge",
        "bridge git",
        "bridge git status",
        "bridge git import",
        "bridge git export",
        "bridge git sync",
        "bridge git reconcile",
        "bridge git push",
        "bridge git pull",
        "bridge git reason",
        "git-overlay",
    ] {
        assert!(
            catalog.command_by_display(display).is_none(),
            "native-only catalog must not advertise git-overlay command `{display}`"
        );
        assert!(
            command_runtime_contract(display).is_none(),
            "native-only runtime contracts must not resolve git-overlay command `{display}`"
        );
    }
}

#[test]
fn json_kind_marks_streaming_command_surfaces() {
    let catalog = build_command_catalog();
    for (display, kind) in [
        ("watch", "jsonl"),
        ("rebase", "jsonl"),
        ("status", "json_or_jsonl"),
        ("thread show", "json_or_jsonl"),
    ] {
        let entry = catalog
            .commands
            .iter()
            .find(|entry| entry.display == display)
            .unwrap_or_else(|| panic!("missing command catalog entry for `{display}`"));
        assert_eq!(
            entry.json_kind, kind,
            "`{display}` must advertise its streaming JSON contract"
        );
    }
}

#[test]
fn hidden_clap_flags_are_present_in_machine_catalog() {
    fn walk_clap_hidden_flags(
        command: &clap::Command,
        path: &mut Vec<String>,
        hidden_flags: &mut Vec<(String, String)>,
    ) {
        let display = path.join(" ");
        for arg in command.get_arguments() {
            if !arg.is_hide_set() {
                continue;
            }
            if arg.get_long().is_some() || arg.get_short().is_some() {
                hidden_flags.push((display.clone(), arg.get_id().as_str().to_string()));
            }
        }

        for subcommand in command.get_subcommands() {
            path.push(subcommand.get_name().to_string());
            walk_clap_hidden_flags(subcommand, path, hidden_flags);
            path.pop();
        }
    }

    let clap = Cli::command();
    let catalog = build_command_catalog();
    let mut hidden_flags = Vec::new();
    walk_clap_hidden_flags(&clap, &mut Vec::new(), &mut hidden_flags);

    let mut failures = Vec::new();
    for (display, id) in hidden_flags {
        if display.is_empty() {
            let Some(option) = catalog.global_options.iter().find(|option| option.id == id) else {
                failures.push(format!("global hidden flag `{id}` missing from catalog"));
                continue;
            };
            if !option.hidden {
                failures.push(format!(
                    "global hidden flag `{id}` is cataloged but hidden=false"
                ));
            }
            continue;
        }

        let Some(entry) = catalog.command_by_display(&display) else {
            failures.push(format!(
                "{display}: hidden flag `{id}` command missing from catalog"
            ));
            continue;
        };
        let Some(option) = entry.options.iter().find(|option| option.id == id) else {
            failures.push(format!(
                "{display}: hidden flag `{id}` missing from catalog options"
            ));
            continue;
        };
        if !option.hidden {
            failures.push(format!(
                "{display}: hidden flag `{id}` is cataloged but hidden=false"
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "every clap `hide = true` flag must remain machine-discoverable in \
         `heddle help --output json` with `hidden: true`:\n  {}",
        failures.join("\n  ")
    );
}

#[test]
fn json_discriminator_table_starts_with_bounded_command_slice() {
    // Wire-format-stable list. PR #251 instrumented the initial set;
    // heddle#272 swept the named-by-persona verbs (stack, goto, fork,
    // revert, purge, redact, stash, clean, discuss, context, review,
    // cherry-pick, bisect); heddle#641 swept the remaining verbs whose
    // runtime JSON already emits `output_kind` (abort, adopt, the agent
    // session verbs, bridge git push/pull, continue, daemon stop,
    // doctor, expand, fetch, land, log,
    // maintenance gc/index, merge --preview, pull, push, query, ready,
    // the remote family, start, switch, sync, and the thread lifecycle
    // verbs). Any further sweep MUST extend this list and document the
    // addition.
    //
    // `clone` appears three times: hosted `clone --output json` emits a
    // preliminary `clone_connection` envelope followed by the final `clone`
    // payload, and `clone --recursive --output json` (Spool epic P9) emits a
    // `clone_monorepo` summary; all three discriminator values are advertised
    // so agents can route on any record. See heddle#272 (PR #281 r3).
    let displays = raw_json_discriminator_specs()
        .iter()
        .map(|(path, _)| path.join(" "))
        .collect::<Vec<_>>();
    assert_eq!(
        displays,
        vec![
            // heddle#641 swept every remaining verb whose runtime JSON
            // already carries `output_kind` (probed live + verified against
            // the emitting structs). The values are the RUNTIME truths, not
            // snake-cased display paths — see the overrides documented in
            // tests/cli_integration/output_kind_invariant.rs.
            "abort",
            "adopt",
            "actor spawn",
            "actor list",
            "actor show",
            "actor explain",
            "actor done",
            "agent serve",
            "agent status",
            "agent stop",
            "agent capture",
            "agent ready",
            "agent task create",
            "agent task list",
            "agent task show",
            "agent task update",
            "agent fanout plan",
            "agent fanout start",
            "auth logout",
            "auth status",
            "auth create-service-token",
            "import git",
            "bridge git status",
            "bridge git import",
            "bridge git sync",
            "bridge git reconcile",
            "bridge git push",
            "bridge git pull",
            "capture",
            "checkpoint",
            "cherry-pick",
            "clean",
            "clone",
            "clone",
            // clone_monorepo discriminator: `clone --recursive --output json`
            // (Spool epic P9) emits a monorepo summary record.
            "clone",
            "expand",
            "commit",
            "continue",
            "context set",
            "context get",
            "context list",
            "context history",
            "context edit",
            "context supersede",
            "context rm",
            "context check",
            "context suggest",
            "context audit",
            "daemon stop",
            "diff",
            "discuss open",
            "discuss append",
            "discuss resolve",
            "discuss list",
            "discuss show",
            "doctor",
            "doctor docs",
            "doctor schemas",
            "fetch",
            "oplog recover",
            "help",
            "init",
            // `log` appears three times: the entry advertises `log`,
            // `log --reflog`, and `log --timeline` variants, mirroring
            // `undo`/`clone`.
            "log",
            "log",
            "log",
            "maintenance gc",
            "maintenance index",
            "merge",
            "pull",
            "push",
            "query",
            "query",
            "ready",
            "rebase",
            "redact apply",
            "redact list",
            "redact show",
            "redact purge apply",
            "redact purge list",
            "redact trust add",
            "redact trust list",
            "redact trust remove",
            "remote list",
            "remote add",
            "remote remove",
            "remote set-default",
            "remote show",
            "resolve",
            "revert",
            "review show",
            "review sign",
            "review next",
            "review health",
            "schemas",
            "land",
            "show",
            "start",
            "stash list",
            "stash show",
            "status",
            "support grant",
            "support list",
            "support revoke",
            "switch",
            "sync",
            "thread create",
            "thread switch",
            "thread list",
            "thread show",
            "thread rename",
            "thread refresh",
            "thread resolve",
            "thread promote",
            "thread drop",
            "thread revoke-approval",
            "thread cleanup",
            "thread marker list",
            "thread marker create",
            "thread marker delete",
            "thread marker show",
            "timeline status",
            "timeline record-start",
            "timeline record-finish",
            "timeline fork",
            "timeline reset",
            "timeline recover",
            "verify",
            "visibility set",
            "visibility promote",
            "visibility show",
            "visibility list",
            "undo",
            "undo",
            "undo",
        ]
    );
}

/// heddle#641 close-the-class conformance: every schema verb whose
/// JSON schema declares an `output_kind` property MUST have a
/// catalog discriminator whose value matches the schema's const.
///
/// Why this closes the gap: `schema_for_verb` only injects the
/// `output_kind` enum-const into a schema when the catalog
/// advertises a discriminator for that verb. A verb whose mirror
/// struct declares `output_kind` (because the runtime payload
/// emits it) but whose catalog entry lacks the discriminator
/// therefore surfaces here as a *const-less* `output_kind`
/// property — exactly the advertise-nothing gap that left ~111
/// schema-bearing commands with `json_discriminators: []`. A new
/// command that emits `output_kind` without registering its
/// discriminator fails this test; one that registers a
/// discriminator that diverges from the schema const fails the
/// mismatch arm. Runtime-vs-catalog equality is enforced
/// separately by `tests/cli_integration/output_kind_invariant.rs`.
#[test]
fn schema_output_kind_discriminators_are_complete_and_consistent() {
    use std::collections::BTreeSet;

    use crate::cli::commands::schema_for_verb;

    fn resolve_schema_ref<'a>(
        root: &'a serde_json::Value,
        reference: &str,
    ) -> &'a serde_json::Value {
        reference
            .strip_prefix("#/$defs/")
            .or_else(|| reference.strip_prefix("#/definitions/"))
            .and_then(|name| {
                root.get("$defs")
                    .or_else(|| root.get("definitions"))
                    .and_then(|defs| defs.get(name))
            })
            .unwrap_or_else(|| panic!("schema reference `{reference}` resolves"))
    }

    fn collect_output_kind_values<'a>(
        root: &'a serde_json::Value,
        schema: &'a serde_json::Value,
        values: &mut BTreeSet<String>,
    ) {
        if let Some(reference) = schema.get("$ref").and_then(|value| value.as_str()) {
            collect_output_kind_values(root, resolve_schema_ref(root, reference), values);
            return;
        }

        if let Some(enum_values) = schema
            .get("properties")
            .and_then(|properties| properties.get("output_kind"))
            .and_then(|property| property.get("enum"))
            .and_then(|values| values.as_array())
        {
            values.extend(
                enum_values
                    .iter()
                    .filter_map(|value| value.as_str())
                    .map(str::to_string),
            );
        } else if let Some(value) = schema
            .get("properties")
            .and_then(|properties| properties.get("output_kind"))
            .and_then(|property| property.get("const"))
            .and_then(|value| value.as_str())
        {
            values.insert(value.to_string());
        }

        for combinator in ["anyOf", "oneOf", "allOf"] {
            if let Some(schemas) = schema.get(combinator).and_then(|value| value.as_array()) {
                for schema in schemas {
                    collect_output_kind_values(root, schema, values);
                }
            }
        }
    }

    let mut missing = Vec::new();
    let mut mismatched = Vec::new();
    let mut checked = 0usize;

    for verb in schema_verbs() {
        let Some(schema) = schema_for_verb(verb) else {
            panic!("catalog schema verb `{verb}` has no registered schema");
        };
        let mut actual = BTreeSet::new();
        collect_output_kind_values(&schema, &schema, &mut actual);
        if actual.is_empty() {
            // The schema does not declare `output_kind` — the verb's
            // runtime payload genuinely lacks the discriminator (the
            // UNSWEPT_TODO rolldown in output_kind_invariant.rs).
            continue;
        };
        checked += 1;

        let mut expected_discriminators = command_json_discriminators_for_schema_verb(verb);
        if schema.get("anyOf").is_some() {
            expected_discriminators.extend(command_json_discriminators().into_iter().filter(
                |discriminator| {
                    discriminator.display == verb
                        && discriminator.schema_verb.as_deref() != Some(verb)
                },
            ));
        }
        let expected = expected_discriminators
            .into_iter()
            .filter(|discriminator| discriminator.field == "output_kind")
            .map(|discriminator| discriminator.value)
            .collect::<BTreeSet<_>>();
        if expected.is_empty() {
            missing.push(format!(
                "`{verb}`: schema declares an `output_kind` property but the \
                 catalog advertises no json_discriminator for it"
            ));
            continue;
        }

        if actual != expected {
            mismatched.push(format!(
                "`{verb}`: schema output_kind values {actual:?} != catalog discriminators {expected:?}"
            ));
        }
    }

    assert!(
        missing.is_empty() && mismatched.is_empty(),
        "Catalog json_discriminators drift from the schema `output_kind` contract. \
         Register the discriminator with `json_discriminator(Some(\"<verb>\"), \
         \"output_kind\", \"<runtime value>\")` on the command's catalog entry \
         (the value must match what the command actually emits).\n\nMissing:\n  - {}\n\nMismatched:\n  - {}",
        missing.join("\n  - "),
        mismatched.join("\n  - ")
    );
    assert!(
        checked >= 90,
        "expected the conformance sweep to inspect the full discriminator \
         surface (~95 schema verbs declare `output_kind` after #473 phases 1-2); only {checked} \
         were checked — the schema injection or verb collection likely regressed"
    );
}

#[test]
fn json_discriminator_metadata_is_internally_consistent() {
    let raw_discriminators = raw_json_discriminator_specs();
    // A single command path MAY advertise more than one
    // discriminator (e.g. `clone` carries both `clone` and
    // `clone_connection` because hosted `clone --output json`
    // emits a preliminary connection envelope before the final
    // payload — see heddle#272). But each (path, value) pair must
    // still be unique, otherwise two entries would advertise the
    // same wire-format token and agents couldn't tell them apart.
    let path_value_pairs = raw_discriminators
        .iter()
        .map(|(path, d)| (path.to_vec(), d.value))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        path_value_pairs.len(),
        raw_discriminators.len(),
        "JSON discriminator table contains duplicate (path, value) pairs"
    );

    let mut schema_verb_values = std::collections::BTreeMap::new();
    for (path, discriminator) in raw_discriminators {
        let display = path.join(" ");
        let contract = raw_command_contract_for_path(path.iter().copied())
            .unwrap_or_else(|| panic!("JSON discriminator references unknown `{display}`"));
        assert!(
            contract.supports_json,
            "`{display}` advertises JSON discriminator `{}` but does not support JSON",
            discriminator.value
        );
        assert!(
            matches!(discriminator.field, "kind" | "output_kind"),
            "`{display}` advertises unsupported discriminator field `{}`",
            discriminator.field
        );
        assert!(
            !discriminator.value.is_empty(),
            "`{display}` discriminator value must be non-empty"
        );

        if let Some(schema_verb) = discriminator.schema_verb {
            let schema_value = (discriminator.field, discriminator.value);
            if let Some(previous) = schema_verb_values.insert(schema_verb, schema_value) {
                assert_eq!(
                    previous, schema_value,
                    "JSON discriminator schema verb `{schema_verb}` is registered with \
                     conflicting discriminator values"
                );
            }
            assert!(
                contract_schema_verbs(contract).any(|verb| verb == schema_verb),
                "`{display}` advertises discriminator schema verb `{schema_verb}` not present in its command contract"
            );
            assert!(
                discriminator.no_schema_reason.is_none(),
                "`{display}` cannot have both a schema verb and a no-schema reason"
            );
        } else {
            assert!(
                discriminator
                    .no_schema_reason
                    .is_some_and(|reason| !reason.is_empty()),
                "`{display}` discriminator without a schema verb must document why"
            );
        }
    }
}

#[test]
fn command_catalog_exposes_active_json_discriminator_metadata() {
    let catalog = build_command_catalog();
    let active = command_json_discriminators();
    for discriminator in &active {
        let entry = catalog
            .commands
            .iter()
            .find(|command| command.display == discriminator.display)
            .unwrap_or_else(|| {
                panic!(
                    "active JSON discriminator references missing command `{}`",
                    discriminator.display
                )
            });
        assert!(entry.supports_json);
        assert!(
            entry
                .json_discriminators
                .iter()
                .any(|entry_discriminator| entry_discriminator == discriminator),
            "`{}` catalog entry must expose its JSON discriminator metadata",
            discriminator.display
        );
    }

    let status = catalog
        .commands
        .iter()
        .find(|entry| entry.display == "status")
        .expect("status should be cataloged");
    assert_eq!(status.json_discriminators.len(), 1);
    assert_eq!(status.json_discriminators[0].field, "output_kind");
    assert_eq!(status.json_discriminators[0].value, "status");
}

fn raw_json_discriminator_specs() -> Vec<(&'static [&'static str], CommandJsonDiscriminatorSpec)> {
    CONTRACTS
        .iter()
        .flat_map(|entry| {
            contract_json_discriminators(entry.contract)
                .map(move |discriminator| (entry.path, discriminator))
        })
        .collect()
}

#[test]
fn catalog_option_lookup_includes_globals_and_finite_values() {
    let catalog = build_command_catalog();

    let start_options = catalog
        .options_for_display("start")
        .expect("start should be cataloged");
    let output = start_options
        .iter()
        .find(|option| option.long.as_deref() == Some("output"))
        .expect("global --output should be included in command options");
    assert_eq!(output.possible_values, vec!["json", "json-compact", "text"]);
    for command in &catalog.commands {
        assert!(
            !command
                .options
                .iter()
                .any(|option| option.long.as_deref() == Some("json")),
            "legacy --json should not be included in command options for {}",
            command.path.join(" ")
        );
    }
    assert!(
        start_options
            .iter()
            .any(|option| option.long.as_deref() == Some("help")),
        "generated --help should be included in command options"
    );

    let workspace = start_options
        .iter()
        .find(|option| option.long.as_deref() == Some("workspace"))
        .expect("start --workspace should be cataloged");
    assert_eq!(
        workspace.possible_values,
        vec!["auto", "materialized", "virtualized", "solid"]
    );

    let context_set_options = catalog
        .options_for_path(&["context".to_string(), "set".to_string()])
        .expect("context set should be cataloged");
    let scope = context_set_options
        .iter()
        .find(|option| option.long.as_deref() == Some("scope"))
        .expect("context set --scope should be cataloged");
    assert!(
        scope.possible_values.is_empty(),
        "context scope accepts open-ended values like symbol:<name>"
    );
    let kind = context_set_options
        .iter()
        .find(|option| option.long.as_deref() == Some("kind"))
        .expect("context set --kind should be cataloged");
    assert_eq!(
        kind.possible_values,
        vec!["constraint", "invariant", "rationale"]
    );

    let fsck_options = catalog
        .options_for_display("fsck")
        .expect("fsck should be cataloged");
    let repair = fsck_options
        .iter()
        .find(|option| option.long.as_deref() == Some("repair"))
        .expect("fsck --repair should be cataloged");
    assert_eq!(repair.possible_values, vec!["git"]);

    let integration_install_options = catalog
        .options_for_display("integration install")
        .expect("integration install should be cataloged");
    let scope = integration_install_options
        .iter()
        .find(|option| option.long.as_deref() == Some("scope"))
        .expect("integration install --scope should be cataloged");
    assert_eq!(scope.possible_values, vec!["repo", "user"]);
    assert_eq!(scope.aliases, vec!["harness-install-scope"]);
}

#[test]
fn command_contract_table_drives_help_tiers() {
    let catalog = build_command_catalog();
    for (display, tier, surface, visibility, canonical, canonical_kind, executable) in [
        (
            "status", "everyday", "native", "everyday", None, None, false,
        ),
        (
            "verify", "everyday", "native", "everyday", None, None, false,
        ),
        (
            "commit", "everyday", "native", "everyday", None, None, false,
        ),
        ("land", "everyday", "native", "everyday", None, None, false),
        ("push", "everyday", "native", "everyday", None, None, false),
        (
            "capture", "advanced", "native", "advanced", None, None, false,
        ),
        (
            "thread create",
            "advanced",
            "native",
            "advanced",
            None,
            None,
            false,
        ),
        (
            "thread promote",
            "advanced",
            "native",
            "advanced",
            None,
            None,
            false,
        ),
        (
            "checkpoint",
            "advanced",
            "native",
            "advanced",
            None,
            None,
            false,
        ),
        (
            "switch",
            "advanced",
            "git_adapter",
            "git_adapter",
            Some("thread switch"),
            Some("direct_command"),
            false,
        ),
    ] {
        let entry = catalog
            .commands
            .iter()
            .find(|entry| entry.display == display)
            .unwrap_or_else(|| panic!("missing command catalog entry for `{display}`"));
        assert_eq!(entry.tier, tier);
        assert_eq!(entry.surface, surface);
        assert_eq!(entry.help_visibility, visibility);
        assert_eq!(entry.canonical_command.as_deref(), canonical);
        assert_eq!(
            entry
                .canonical_action
                .as_ref()
                .map(|action| action.kind.as_str()),
            canonical_kind
        );
        assert_eq!(
            entry
                .canonical_action
                .as_ref()
                .is_some_and(|action| action.executable),
            executable
        );
        assert_eq!(command_help_tier(display), tier);
        assert_eq!(command_surface(display), surface);
        assert_eq!(command_help_visibility(display), visibility);
        assert_eq!(command_canonical_command(display), canonical);
    }
    for (display, canonical, kind) in [
        ("stash pop", "undo", "conceptual_home"),
        ("fetch", "pull", "workflow"),
    ] {
        let entry = catalog
            .commands
            .iter()
            .find(|entry| entry.display == display)
            .unwrap_or_else(|| panic!("missing command catalog entry for `{display}`"));
        let action = entry
            .canonical_action
            .as_ref()
            .unwrap_or_else(|| panic!("`{display}` should expose a canonical action"));
        assert_eq!(action.command, canonical);
        assert_eq!(action.kind, kind);
        assert!(
            !action.executable,
            "`{display}` is not a direct command replacement"
        );
        assert!(!action.note.is_empty());
    }
    assert_eq!(command_help_tier("transaction"), "hidden");

    let thread_list = catalog
        .commands
        .iter()
        .find(|entry| entry.display == "thread list")
        .expect("thread list should be cataloged");
    assert_eq!(thread_list.tier, "advanced");
}

#[test]
fn parsed_command_op_id_support_reads_contract_table() {
    for (argv, expected) in [
        (vec!["heddle", "status"], false),
        (vec!["heddle", "commit", "-m", "checkpoint"], true),
        (vec!["heddle", "thread", "list"], false),
        (vec!["heddle", "thread", "drop", "feature"], true),
    ] {
        let cli = Cli::try_parse_from(argv.clone())
            .unwrap_or_else(|err| panic!("failed to parse {argv:?}: {err}"));
        let display = command_path(&cli.command).join(" ");
        assert_eq!(
            command_supports_op_id_for_command(&cli.command),
            expected,
            "`{display}` op-id support must come from its parsed command contract"
        );
        assert_eq!(
            command_supports_op_id(&display),
            expected,
            "`{display}` string lookup must agree with parsed command contract"
        );
    }
}

#[test]
fn parsed_command_json_support_reads_contract_table() {
    for (argv, expected) in [
        (vec!["heddle", "status"], true),
        (vec!["heddle", "help"], true),
        (vec!["heddle", "shell", "completion", "bash"], false),
        (vec!["heddle", "thread", "cd", "feature"], false),
    ] {
        let cli = Cli::try_parse_from(argv.clone())
            .unwrap_or_else(|err| panic!("failed to parse {argv:?}: {err}"));
        let display = command_path(&cli.command).join(" ");
        let entry = build_command_catalog()
            .commands
            .into_iter()
            .find(|entry| entry.display == display)
            .unwrap_or_else(|| panic!("missing command catalog entry for `{display}`"));
        assert_eq!(
            command_supports_json_for_command(&cli.command),
            expected,
            "`{display}` JSON support must come from its parsed command contract"
        );
        assert_eq!(entry.supports_json, expected);
    }
}

#[test]
fn parsed_command_runtime_contract_exposes_catalog_fields() {
    let cli = Cli::try_parse_from(["heddle", "thread", "drop", "feature"])
        .expect("thread drop should parse");
    let runtime = command_runtime_contract_for_command(&cli.command);
    let catalog = build_command_catalog();
    let entry = catalog
        .commands
        .iter()
        .find(|entry| entry.display == runtime.display)
        .expect("runtime command should be present in catalog");

    assert_eq!(runtime.path, vec!["thread", "drop"]);
    assert_eq!(runtime.supports_json, entry.supports_json);
    assert_eq!(runtime.supports_op_id, entry.supports_op_id);
    assert_eq!(runtime.persists_op_id, entry.persists_op_id);
    assert_eq!(
        runtime.uses_bootstrap_op_id_store,
        entry.op_id_store_scope == "bootstrap"
    );
    assert_eq!(runtime.help_visibility, entry.help_visibility);
    assert_eq!(runtime.help_rank, entry.help_rank);
    assert_eq!(runtime.surface, entry.surface);
}

#[test]
fn op_id_persistence_reads_contract_table() {
    let catalog = build_command_catalog();
    for (display, persists, store_scope) in [
        ("capture", false, "repository"),
        ("review sign", false, "repository"),
        ("commit", false, "repository"),
        ("status", false, "none"),
        ("init", false, "bootstrap"),
        ("adopt", false, "bootstrap"),
        ("clone", false, "bootstrap"),
    ] {
        let entry = catalog
            .commands
            .iter()
            .find(|entry| entry.display == display)
            .unwrap_or_else(|| panic!("missing command catalog entry for `{display}`"));
        assert_eq!(
            entry.persists_op_id, persists,
            "`{display}` op-id persistence must be cataloged"
        );
        assert_eq!(
            entry.op_id_store_scope, store_scope,
            "`{display}` op-id store scope must be cataloged"
        );
        assert_eq!(
            command_persists_op_id(display),
            persists,
            "`{display}` runtime op-id persistence must come from the contract table"
        );
        assert_eq!(
            command_uses_bootstrap_op_id_store(display),
            store_scope == "bootstrap",
            "`{display}` runtime op-id store scope must come from the contract table"
        );
        if persists {
            assert!(
                entry.supports_op_id,
                "`{display}` cannot persist op-ids unless it supports op-id replay"
            );
        }
    }
}

#[test]
fn feature_gated_command_roots_are_catalog_owned() {
    assert_eq!(
        feature_gated_command_roots(),
        &["auth", "presence", "spool", "support"]
    );
}
