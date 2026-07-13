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
// Git projection grouping rows, and `redact trust`.
const RUNTIME_CONTRACT_PARSE_SAMPLES: &[RuntimeContractParseSample] = &[
    sample(&["abort"], &["abort"]),
    sample(&["adopt"], &["adopt"]),
    sample(
        &["agent", "presence", "list"],
        &["agent", "presence", "list"],
    ),
    sample(
        &["agent", "presence", "show"],
        &["agent", "presence", "show"],
    ),
    sample(
        &["agent", "presence", "explain"],
        &["agent", "presence", "explain"],
    ),
    sample(
        &["agent", "presence", "complete"],
        &["agent", "presence", "complete"],
    ),
    sample(&["agent", "serve"], &["agent", "serve"]),
    sample(&["agent", "status"], &["agent", "status"]),
    sample(&["agent", "stop"], &["agent", "stop"]),
    sample(
        &["agent", "reserve"],
        &["agent", "reserve", "--thread", "main"],
    ),
    sample(
        &["agent", "heartbeat"],
        &[
            "agent",
            "heartbeat",
            "--lease",
            "lease-1",
            "--token",
            "token",
        ],
    ),
    sample(
        &["agent", "capture"],
        &["agent", "capture", "--lease", "lease-1", "--token", "token"],
    ),
    sample(
        &["agent", "ready"],
        &["agent", "ready", "--lease", "lease-1", "--token", "token"],
    ),
    sample(
        &["agent", "release"],
        &["agent", "release", "--lease", "lease-1", "--token", "token"],
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
    sample(&["auth", "login"], &["auth", "login", "--open-browser"]),
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
    sample(&["sync", "git"], &["sync", "git"]),
    #[cfg(feature = "ingest")]
    sample(
        &["context", "reason", "git"],
        &["context", "reason", "git", "--path", "."],
    ),
    sample(&["capture"], &["capture"]),
    sample(&["clone"], &["clone", "remote", "local"]),
    sample(
        &["collapse"],
        &["collapse", "s1", "s2", "--into", "squashed"],
    ),
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
    sample(
        &["discuss", "reopen"],
        &[
            "discuss",
            "reopen",
            "discussion-1",
            "--reason",
            "new evidence",
        ],
    ),
    sample(&["discuss", "list"], &["discuss", "list"]),
    sample(&["discuss", "show"], &["discuss", "show", "discussion-1"]),
    sample(&["doctor"], &["doctor"]),
    sample(&["doctor", "docs"], &["doctor", "docs"]),
    sample(&["doctor", "schemas"], &["doctor", "schemas"]),
    sample(&["fsck"], &["fsck"]),
    sample(
        &["fsck", "repair", "git"],
        &["fsck", "repair", "git", "--ref", "main", "--preview"],
    ),
    sample(&["oplog", "recover"], &["oplog", "recover"]),
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
    sample(&["pull"], &["pull"]),
    sample(&["push"], &["push"]),
    sample(&["query"], &["query"]),
    sample(&["ready"], &["ready"]),
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
        &["agent", "provenance", "begin"],
        &[
            "agent",
            "provenance",
            "begin",
            "--provider",
            "openai",
            "--model",
            "gpt-5",
        ],
    ),
    sample(
        &["agent", "provenance", "segment"],
        &[
            "agent",
            "provenance",
            "segment",
            "--provider",
            "openai",
            "--model",
            "gpt-5",
        ],
    ),
    sample(
        &["agent", "provenance", "end"],
        &["agent", "provenance", "end"],
    ),
    sample(
        &["agent", "provenance", "show"],
        &["agent", "provenance", "show"],
    ),
    sample(
        &["agent", "provenance", "list"],
        &["agent", "provenance", "list"],
    ),
    sample(&["shell", "init"], &["shell", "init", "bash"]),
    sample(&["shell", "completion"], &["shell", "completion", "bash"]),
    sample(&["shell", "prompt"], &["shell", "prompt"]),
    sample(&["complete"], &["__complete", "threads"]),
    sample(&["land"], &["land"]),
    sample(&["show"], &["show", "HEAD"]),
    sample(&["start"], &["start", "feature"]),
    sample(&["status"], &["status"]),
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
        "git commit -m \"...\"",
        "heddle capture -m \"Preserve raw Git operation work\"",
        "git switch <branch>",
        "heddle start feature/auth --path <dir>",
        "heddle clone <remote> <fresh-path>",
        "heddle clone <local-path> <path>",
        "heddle clone /tmp/source <path> --thread main",
        "heddle import git --path <full-git-repo> --ref <ref>",
        "heddle thread promote main",
        "heddle thread resolve main",
    ] {
        validate_recommended_action(action)
            .unwrap_or_else(|err| panic!("expected `{action}` to validate: {err}"));
    }
    #[cfg(feature = "git-overlay")]
    {
        for action in [
            "heddle import git --ref main",
            "heddle import git --ref origin/main",
            "heddle ready --thread origin/main",
            "heddle fsck repair git --ref main --preview",
            "heddle fsck repair git --prefer heddle --ref main --preview",
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
        .find(|template| template.action == "git commit -m \"...\"")
        .expect("commit placeholder should have a structured template");
    assert_eq!(
        commit.argv_template,
        vec!["git", "commit", "-m", "<message>"]
    );
    assert_eq!(commit.required_inputs, vec!["message"]);
    assert!(commit.agent_may_fill);

    let switch = recommended_action_template("git switch <branch>")
        .expect("switch placeholder should resolve");
    assert_eq!(switch.argv_template, vec!["git", "switch", "<branch>"]);
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
        recommended_action_template("heddle import git --path <full-git-repo> --ref <ref>")
            .expect("shallow import recovery placeholder should resolve");
    assert_eq!(
        import.argv_template,
        vec![
            "heddle",
            "import",
            "git",
            "--path",
            "<full-git-repo>",
            "--ref",
            "<ref>"
        ]
    );
    assert_eq!(import.required_inputs, vec!["path", "ref"]);
    assert!(!import.agent_may_fill);

    let merge = recommended_action_template("heddle land --thread <thread>")
        .expect("land recovery placeholder should resolve");
    assert_eq!(
        merge.argv_template,
        vec!["heddle", "land", "--thread", "<thread>"]
    );
    assert_eq!(merge.required_inputs, vec!["thread"]);
    assert!(!merge.agent_may_fill);
}

#[test]
fn action_fields_template_dirty_worktree_message_placeholders() {
    for (action, expected_argv_template) in [(
        "heddle capture -m \"...\"",
        vec!["heddle", "capture", "-m", "<message>"],
    )] {
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
    for (action, expected_argv_template) in [(
        "heddle capture -m ...",
        vec!["heddle", "capture", "-m", "<message>"],
    )] {
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
    let err = validate_recommended_action("git switch <missing-template>")
        .expect_err("unregistered display placeholder should fail validation");
    assert!(
        err.contains("structured template"),
        "error should explain missing template: {err}"
    );

    assert!(
        recommended_action_template("git switch <missing-template>").is_none(),
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

    validate_recommended_action("git status").expect("Git-owned actions are valid guidance");
}

#[test]
fn contracted_commands_are_not_parseable() {
    for command in [
        "clean",
        "fetch",
        "git-overlay",
        "merge",
        "presence",
        "prove",
        "rebase",
        "spool",
        "stash",
        "support",
        "switch",
    ] {
        assert!(
            Cli::try_parse_from(["heddle", command]).is_err(),
            "removed command `{command}` must not return to the public CLI"
        );
    }
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
    let panic = std::panic::catch_unwind(|| ActionFields::from_action("curl example.com"));
    assert!(
        panic.is_err(),
        "ActionFields must not silently erase invalid action sidecars"
    );
}

#[test]
fn recommended_action_parser_supports_shell_quoted_arguments() {
    let template = recommended_action_template("heddle ready --thread 'feature with spaces'")
        .expect("single-quoted thread action should resolve to a template");
    assert_eq!(
        template.argv_template[1..],
        ["ready", "--thread", "feature with spaces"]
    );

    let template = recommended_action_template("heddle ready --thread 'feature '\\''quoted'\\'''")
        .expect("shell-quoted apostrophe should resolve to a template");
    assert_eq!(
        template.argv_template[1..],
        ["ready", "--thread", "feature 'quoted'"]
    );
}

#[test]
fn checked_action_builder_quotes_and_validates_from_argv() {
    let action = heddle_action(["ready", "--thread", "feature with spaces"]);
    assert_eq!(action, "heddle ready --thread 'feature with spaces'");
    let template =
        recommended_action_template(&action).expect("built action should resolve to a template");
    assert_eq!(
        template.argv_template[1..],
        ["ready", "--thread", "feature with spaces"]
    );

    let git = checked_action_from_argv(["git", "status"]);
    assert_eq!(git, "git status");

    let panic = std::panic::catch_unwind(|| checked_action_from_argv(["curl", "example.com"]));
    assert!(
        panic.is_err(),
        "unowned executables should not enter runtime advice sidecars"
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

#[test]
fn leaf_catalog_entries_publish_exact_output_modes() {
    let catalog = build_command_catalog();
    for command in catalog
        .commands
        .iter()
        .filter(|entry| !entry.has_subcommands)
    {
        assert_eq!(
            command.output_modes.first().map(String::as_str),
            Some("text")
        );
        assert_eq!(
            command.output_modes.iter().any(|mode| mode == "json"),
            command.supports_json,
            "{} must expose its JSON support without trial execution",
            command.display,
        );
        assert_eq!(
            command
                .output_modes
                .iter()
                .any(|mode| mode == "json-compact"),
            raw_command_contract_for_path(command.path.iter().map(String::as_str))
                .expect("catalog entries have command contracts")
                .supports_json_compact,
            "{} must expose its compact projection without trial execution",
            command.display,
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
                "native" | "git_projection" | "automation" | "admin" | "internal"
            ),
            "`{display}` has unknown product surface `{}`",
            contract.surface
        );
        assert!(
            matches!(
                contract.help_visibility,
                "everyday" | "advanced" | "git_projection" | "hidden"
            ),
            "`{display}` has unknown help visibility `{}`",
            contract.help_visibility
        );
        if contract.help_visibility == "git_projection" {
            assert_eq!(
                contract.surface, "git_projection",
                "`{display}` Git projection commands must live on the Git projection surface"
            );
            assert!(
                contract.canonical_command.is_some(),
                "`{display}` Git projection commands must name a canonical Heddle command"
            );
            assert!(
                contract.canonical_kind.is_some(),
                "`{display}` Git projection commands must classify the canonical action"
            );
            assert!(
                contract.canonical_note.is_some(),
                "`{display}` Git projection commands must explain the canonical action"
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
                    && !contract.writes_metadata
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
                vec![CommandSideEffect::ObserveOnly],
                "`{display}` observe-only side_effects must stay exact"
            );
        } else {
            for (flag, effect) in [
                (contract.may_initialize, CommandSideEffect::Initialize),
                (contract.may_import_git, CommandSideEffect::ImportGit),
                (
                    contract.writes_heddle_refs,
                    CommandSideEffect::WritesHeddleRefs,
                ),
                (contract.writes_git_refs, CommandSideEffect::WritesGitRefs),
                (contract.writes_worktree, CommandSideEffect::WritesWorktree),
                (contract.writes_metadata, CommandSideEffect::WritesMetadata),
                (contract.writes_config, CommandSideEffect::WritesConfig),
                (contract.writes_hooks, CommandSideEffect::WritesHooks),
                (contract.network_io, CommandSideEffect::NetworkIo),
                (contract.daemon_process, CommandSideEffect::DaemonProcess),
                (contract.object_gc, CommandSideEffect::ObjectGc),
                (
                    contract.external_command,
                    CommandSideEffect::ExternalCommand,
                ),
                (
                    contract.destructive_requires_force,
                    CommandSideEffect::DestructiveRequiresForce,
                ),
                (
                    contract.destructive_data,
                    CommandSideEffect::DestructiveData,
                ),
            ] {
                assert_eq!(
                    effects.contains(&effect),
                    flag,
                    "`{display}` side_effects must mirror `{effect:?}`"
                );
            }
            if contract.may_write_worktree && !contract.writes_worktree {
                assert!(
                    effects.contains(&CommandSideEffect::MayWriteWorktree),
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

#[test]
fn every_mutating_leaf_declares_a_concrete_side_effect() {
    let paths_with_children = contract_paths_with_children(&CONTRACTS.iter().collect::<Vec<_>>());

    for entry in CONTRACTS {
        if paths_with_children.contains(&entry.path.to_vec()) || !entry.contract.mutates {
            continue;
        }
        let declares_concrete_effect = entry.contract.writes_heddle_refs
            || entry.contract.writes_git_refs
            || entry.contract.writes_worktree
            || entry.contract.writes_metadata
            || entry.contract.writes_config
            || entry.contract.writes_hooks
            || entry.contract.network_io
            || entry.contract.daemon_process
            || entry.contract.object_gc
            || entry.contract.external_command;
        assert!(
            declares_concrete_effect,
            "mutating leaf `{}` must declare a concrete write, process, or network effect",
            entry.path.join(" ")
        );
        let effects = side_effects(entry.contract);
        assert!(
            !effects.is_empty(),
            "mutating leaf `{}` must declare a concrete side effect",
            entry.path.join(" ")
        );
        assert_ne!(
            side_effect_class(entry.contract),
            "none",
            "mutating leaf `{}` must have a concrete side-effect class",
            entry.path.join(" ")
        );
    }
}

#[test]
fn observe_only_leaves_declare_no_mutation_effects() {
    let paths_with_children = contract_paths_with_children(&CONTRACTS.iter().collect::<Vec<_>>());

    for entry in CONTRACTS {
        if paths_with_children.contains(&entry.path.to_vec()) || !entry.contract.observe_only {
            continue;
        }
        assert_eq!(
            side_effects(entry.contract),
            vec![CommandSideEffect::ObserveOnly],
            "observe-only leaf `{}` must declare no mutation effects",
            entry.path.join(" ")
        );
    }
}

fn assert_command_effects(path: &[&str], expected: &[CommandSideEffect]) {
    let contract = raw_command_contract_for_path(path.iter().copied())
        .unwrap_or_else(|| panic!("missing contract for `{}`", path.join(" ")));
    assert_eq!(side_effects(contract), expected, "`{}`", path.join(" "));
}

#[test]
fn sidecar_only_effect_sets_exclude_refs() {
    for path in [
        &["agent", "presence", "complete"][..],
        &["agent", "heartbeat"],
        &["agent", "release"],
        &["agent", "task", "create"],
        &["agent", "task", "update"],
        &["context", "reason", "git"],
        &["discuss", "open"],
        &["discuss", "append"],
        &["discuss", "resolve"],
        &["discuss", "reopen"],
        &["review", "sign"],
        &["agent", "provenance", "begin"],
        &["agent", "provenance", "segment"],
        &["agent", "provenance", "end"],
        &["timeline", "record-start"],
        &["timeline", "record-finish"],
        &["timeline", "fork"],
        &["timeline", "recover"],
        &["visibility", "set"],
        &["visibility", "promote"],
    ] {
        assert_command_effects(path, &[CommandSideEffect::WritesMetadata]);
    }
}

#[test]
fn state_attached_effect_sets_include_refs() {
    for path in [
        &["context", "set"][..],
        &["context", "edit"],
        &["context", "supersede"],
        &["context", "rm"],
    ] {
        assert_command_effects(
            path,
            &[
                CommandSideEffect::WritesHeddleRefs,
                CommandSideEffect::WritesMetadata,
            ],
        );
    }
}

#[test]
fn materializer_effect_sets_include_refs_metadata_and_worktree() {
    for path in [&["timeline", "reset"][..], &["integration", "relay"]] {
        assert_command_effects(
            path,
            &[
                CommandSideEffect::WritesHeddleRefs,
                CommandSideEffect::WritesWorktree,
                CommandSideEffect::WritesMetadata,
            ],
        );
    }
}

#[test]
fn integration_installer_effect_sets_include_config_and_hooks() {
    for path in [
        &["integration", "install"][..],
        &["integration", "uninstall"],
        &["integration", "upgrade"],
    ] {
        assert_command_effects(
            path,
            &[
                CommandSideEffect::WritesMetadata,
                CommandSideEffect::WritesConfig,
                CommandSideEffect::WritesHooks,
            ],
        );
    }
}

#[test]
fn credential_and_trust_effect_sets_are_config_scoped() {
    for path in [
        &["auth", "logout"][..],
        &["redact", "trust", "add"],
        &["redact", "trust", "remove"],
    ] {
        assert_command_effects(path, &[CommandSideEffect::WritesConfig]);
    }
    assert_command_effects(
        &["auth", "login"],
        &[
            CommandSideEffect::WritesConfig,
            CommandSideEffect::NetworkIo,
        ],
    );
}

#[test]
fn fsck_observation_has_no_side_effects() {
    assert_command_effects(&["fsck"], &[]);
    let contract = raw_command_contract_for_path(["fsck"]).expect("fsck contract");
    assert!(contract.observe_only);
    assert!(!contract.supports_op_id);
}

#[test]
fn fsck_repair_git_has_explicit_mutation_effects() {
    assert_command_effects(
        &["fsck", "repair", "git"],
        &[
            CommandSideEffect::ImportGit,
            CommandSideEffect::WritesHeddleRefs,
            CommandSideEffect::WritesGitRefs,
            CommandSideEffect::WritesWorktree,
            CommandSideEffect::WritesMetadata,
        ],
    );
    let contract =
        raw_command_contract_for_path(["fsck", "repair", "git"]).expect("fsck repair git contract");
    assert!(contract.supports_op_id);
    assert!(!contract.persists_op_id);
}

#[test]
fn sync_git_adopt_note_is_authority_neutral() {
    let contract = raw_command_contract_for_path(["sync", "git"]).expect("sync git contract");
    assert_eq!(
        contract.canonical_note,
        Some(
            "Use adopt to initialize Heddle from an existing Git repository and import its history."
        )
    );
}

#[cfg(not(feature = "git-overlay"))]
#[test]
fn native_only_catalog_excludes_git_overlay_commands() {
    let catalog = build_command_catalog();
    for display in [
        "bridge",
        "status",
        "sync git",
        "context reason git",
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
    // bisect); heddle#641 swept the remaining verbs whose
    // runtime JSON already emits `output_kind` (abort, adopt, the agent
    // session verbs, continue, daemon stop,
    // doctor, expand, fetch, land, log,
    // maintenance gc/index, pull, push, query, ready,
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
            "agent presence list",
            "agent presence show",
            "agent presence explain",
            "agent presence complete",
            "auth logout",
            "auth status",
            "auth create-service-token",
            "import git",
            "export git",
            "sync git",
            "capture",
            "clone",
            "clone",
            // clone_monorepo discriminator: `clone --recursive --output json`
            // (Spool epic P9) emits a monorepo summary record.
            "clone",
            "expand",
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
            "discuss reopen",
            "discuss list",
            "discuss show",
            "doctor",
            "doctor docs",
            "doctor schemas",
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
            "pull",
            "push",
            "query",
            "query",
            "ready",
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
            "status",
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
    for mutation_option in ["ref", "prefer", "preview"] {
        assert!(
            fsck_options
                .iter()
                .all(|option| option.long.as_deref() != Some(mutation_option)),
            "bare fsck must not expose --{mutation_option}"
        );
    }
    let repair_options = catalog
        .options_for_display("fsck repair git")
        .expect("fsck repair git should be cataloged");
    let prefer = repair_options
        .iter()
        .find(|option| option.long.as_deref() == Some("prefer"))
        .expect("fsck repair git --prefer should be cataloged");
    assert_eq!(prefer.possible_values, vec!["git", "heddle"]);
    for expected in ["ref", "preview"] {
        assert!(
            repair_options
                .iter()
                .any(|option| option.long.as_deref() == Some(expected)),
            "fsck repair git --{expected} should be cataloged"
        );
    }

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
        ("land", "everyday", "native", "everyday", None, None, false),
        ("push", "everyday", "native", "everyday", None, None, false),
        (
            "capture", "everyday", "native", "everyday", None, None, false,
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
        (vec!["heddle", "capture", "-m", "checkpoint"], true),
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
    assert_eq!(feature_gated_command_roots(), &["auth"]);
}
