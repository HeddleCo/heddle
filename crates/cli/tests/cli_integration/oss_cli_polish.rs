// SPDX-License-Identifier: Apache-2.0
use clap::CommandFactory;
use cli::cli::Cli;
use repo::operation_dedup::{OperationDedupStore, hash_request_body};

use super::*;

#[test]
fn git_overlay_guide_is_concise_and_actionable() {
    let help = heddle(&["help", "git-overlay"], None).unwrap();
    assert!(
        help.contains("Show the low-friction Git-overlay workflow"),
        "help should discover the guide command: {help}"
    );

    let output = heddle(&["--output", "text", "git-overlay"], None).unwrap();

    assert!(
        output.contains("Git-overlay quick start"),
        "guide should have a clear title: {output}"
    );
    assert!(
        output.contains("heddle bridge git import --ref <branch>"),
        "guide should teach scoped import using the real verb path: {output}"
    );
    assert!(
        output.contains("heddle start <topic> --path ../<topic>"),
        "guide should teach isolated threads: {output}"
    );
    assert!(
        output.contains("heddle doctor"),
        "guide should point to doctor for recovery: {output}"
    );
}

#[test]
fn json_mode_parse_errors_emit_error_envelope() {
    let output = heddle_output(&["--output", "json", "statuz"], None).expect("invoke heddle");
    assert!(!output.status.success(), "unknown command should fail");
    assert!(
        output.stdout.is_empty(),
        "parse failures in JSON mode must not write stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let parsed: Value = serde_json::from_str(stderr)
        .unwrap_or_else(|err| panic!("stderr should be JSON: {err}: {stderr}"));
    assert_eq!(parsed["kind"], "parse_error");
    assert!(
        parsed["error"].as_str().unwrap_or("").contains("statuz"),
        "parse envelope should preserve clap's command detail: {parsed}"
    );
}

#[test]
fn explicit_json_for_text_only_command_uses_contract_advice() {
    let output = heddle_output(&["--output", "json", "completion", "bash"], None).expect("invoke");
    assert!(
        !output.status.success(),
        "text-only command should reject explicit JSON"
    );
    assert!(
        output.stdout.is_empty(),
        "contract refusal must not write stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value = serde_json::from_str(stderr)
        .unwrap_or_else(|err| panic!("stderr should be JSON: {err}: {stderr}"));
    assert_eq!(envelope["kind"], "json_unsupported");
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle commands --output json")),
        "contract advice should point to command catalog: {stderr}"
    );
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("heddle completion")
                && error.contains("Unsafe:")
                && error.contains("Would change:")
                && error.contains("Preserved:")
                && error.contains("Primary recovery:")),
        "contract advice should use the typed refusal envelope: {stderr}"
    );
}

#[test]
fn command_catalog_exposes_agent_metadata_for_options() {
    let json = heddle(&["--output", "json", "commands"], None).unwrap();
    let parsed: Value = serde_json::from_str(&json).unwrap();
    assert!(
        parsed["recommended_action_placeholders"]
            .as_array()
            .expect("placeholder registry should be cataloged")
            .iter()
            .any(|action| action == "git add <files> && heddle continue"),
        "raw Git recovery placeholders should be explicit: {parsed}"
    );
    let commands = parsed["commands"].as_array().unwrap();
    let status = commands
        .iter()
        .find(|entry| entry["display"] == "status")
        .expect("status command should be cataloged");
    assert_eq!(status["supports_json"], true);
    assert_eq!(status["mutates"], false);
    assert_eq!(status["side_effect_class"], "observe_only");
    assert_eq!(status["first_run_behavior"], "observe_only_no_init");
    assert_eq!(status["json_kind"], "json_or_jsonl");
    assert_eq!(status["schema_verbs"], serde_json::json!(["status"]));
    assert_eq!(
        status["documented_schema_verbs"],
        serde_json::json!(["status"])
    );
    let short = status["options"]
        .as_array()
        .unwrap()
        .iter()
        .find(|option| option["long"] == "short")
        .expect("status --short should be cataloged");
    assert_eq!(short["value_kind"], "boolean");

    let commit = commands
        .iter()
        .find(|entry| entry["display"] == "commit")
        .expect("commit shim should be cataloged");
    assert_eq!(commit["mutates"], true);
    assert_eq!(commit["supports_op_id"], true);
    assert_eq!(commit["persists_op_id"], false);
    assert_eq!(commit["side_effect_class"], "initialize");
    assert_eq!(commit["first_run_behavior"], "may_initialize");

    let capture = commands
        .iter()
        .find(|entry| entry["display"] == "capture")
        .expect("capture command should be cataloged");
    assert_eq!(capture["supports_op_id"], true);
    assert_eq!(capture["persists_op_id"], true);

    let init = commands
        .iter()
        .find(|entry| entry["display"] == "init")
        .expect("init should be cataloged");
    assert_eq!(init["mutates"], true);
    assert_eq!(init["supports_op_id"], false);
    assert_eq!(init["side_effect_class"], "initialize");
    assert_eq!(init["first_run_behavior"], "may_initialize");

    let diff = commands
        .iter()
        .find(|entry| entry["display"] == "diff")
        .expect("diff should be cataloged");
    assert_eq!(diff["side_effect_class"], "observe_only");
    assert_eq!(diff["first_run_behavior"], "observe_only_no_init");
    assert_eq!(diff["schema_verbs"], serde_json::json!(["diff"]));
    assert_eq!(diff["documented_schema_verbs"], serde_json::json!([]));

    let watch = commands
        .iter()
        .find(|entry| entry["display"] == "watch")
        .expect("watch should be cataloged");
    assert_eq!(watch["json_kind"], "jsonl");

    let thread_show = commands
        .iter()
        .find(|entry| entry["display"] == "thread show")
        .expect("thread show should be cataloged");
    assert_eq!(thread_show["json_kind"], "json_or_jsonl");

    let completion = commands
        .iter()
        .find(|entry| entry["display"] == "completion")
        .expect("completion should be cataloged");
    assert_eq!(completion["supports_json"], false);
    assert_eq!(completion["json_kind"], "none");
}

#[test]
fn trust_cold_flow_scripts_assert_required_proof_steps() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("cli crate should be under crates/cli")
        .to_path_buf();
    for script in [
        root.join("scripts/trust-cold-flow-human.sh"),
        root.join("scripts/trust-cold-flow-agent.sh"),
    ] {
        let source = std::fs::read_to_string(&script)
            .unwrap_or_else(|err| panic!("read {}: {err}", script.display()));
        for shape in ["small-app", "large-rust", "complex-git"] {
            assert!(
                source.contains(shape),
                "{} should cover {shape}",
                script.display()
            );
        }
        for proof in [
            "bridge git import",
            "checkpoint",
            "commit",
            "undo",
            "fetch",
            "pull",
            "push",
            "clone",
            "reconcile",
            "start",
            "ready",
            "--preview",
            "blame",
            "assert_final_trust",
            "assert_transcript_claims",
        ] {
            assert!(
                source.contains(proof),
                "{} should assert/run proof step `{proof}`",
                script.display()
            );
        }
    }
}

#[test]
fn op_id_replays_local_mutating_command_and_rejects_arg_conflict() {
    let temp = TempDir::new().unwrap();
    let op_id = "550e8400-e29b-41d4-a716-446655440000";

    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "first\n").unwrap();

    let first = heddle(
        &[
            "--output",
            "json",
            "--op-id",
            op_id,
            "capture",
            "-m",
            "op replay",
        ],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&first).expect("first capture JSON should parse");
    assert!(
        parsed["change_id"]
            .as_str()
            .unwrap_or("")
            .starts_with("hd-"),
        "first capture should return a state id: {parsed}"
    );

    std::fs::write(temp.path().join("tracked.txt"), "second\n").unwrap();
    let replay = heddle(
        &[
            "--output",
            "json",
            "--op-id",
            op_id,
            "capture",
            "-m",
            "op replay",
        ],
        Some(temp.path()),
    )
    .unwrap();
    assert_eq!(replay, first, "same op-id and args should replay exactly");

    let status = heddle(&["--output", "json", "status"], Some(temp.path())).unwrap();
    let status: Value = serde_json::from_str(&status).unwrap();
    assert!(
        status["changes"]["modified"]
            .as_array()
            .is_some_and(|paths| paths.iter().any(|path| path == "tracked.txt")),
        "replayed capture must not execute a second mutation: {status}"
    );

    let conflict = heddle_output(
        &[
            "--output",
            "json",
            "--op-id",
            op_id,
            "capture",
            "-m",
            "different args",
        ],
        Some(temp.path()),
    )
    .expect("invoke conflicting op-id");
    assert!(!conflict.status.success(), "conflicting op-id should fail");
    let stderr = std::str::from_utf8(&conflict.stderr).unwrap();
    let parsed: Value = serde_json::from_str(stderr)
        .unwrap_or_else(|err| panic!("conflict should be a JSON envelope: {err}: {stderr}"));
    assert_eq!(parsed["kind"], "op_id_conflict");
}

#[test]
fn unsupported_op_id_fails_from_command_contract_table() {
    let temp = TempDir::new().unwrap();
    let op_id = "550e8400-e29b-41d4-a716-446655440001";

    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "--op-id", op_id, "status"],
        Some(temp.path()),
    )
    .expect("invoke status with unsupported op-id");
    assert!(
        !output.status.success(),
        "read-only status must reject unsupported op-id"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let parsed: Value = serde_json::from_str(stderr).unwrap_or_else(|err| {
        panic!("unsupported op-id should be a JSON envelope: {err}: {stderr}")
    });
    assert_eq!(parsed["kind"], "op_id_unsupported");
    assert!(
        parsed["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle commands --output json")),
        "unsupported op-id should point to the command catalog: {parsed}"
    );
}

#[test]
fn op_id_replays_terminal_failure_and_reports_in_flight() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let terminal_op_id = "550e8400-e29b-41d4-a716-446655440002";
    let terminal_args = [
        "--output",
        "json",
        "--op-id",
        terminal_op_id,
        "thread",
        "drop",
        "missing-thread",
    ];
    let first = heddle_output(&terminal_args, Some(temp.path())).expect("invoke first failure");
    assert!(
        !first.status.success(),
        "missing thread drop should fail before replay"
    );
    assert!(
        first.stdout.is_empty(),
        "JSON-mode terminal failure should keep stdout quiet: {}",
        String::from_utf8_lossy(&first.stdout)
    );
    let first_stderr = std::str::from_utf8(&first.stderr).unwrap();
    let first_envelope: Value = serde_json::from_str(first_stderr)
        .unwrap_or_else(|err| panic!("first failure should be JSON: {err}: {first_stderr}"));
    assert_eq!(first_envelope["kind"], "thread_not_found");

    let replay = heddle_output(&terminal_args, Some(temp.path())).expect("invoke replay failure");
    assert_eq!(
        replay.status.code(),
        first.status.code(),
        "terminal op-id replay should preserve the original exit code"
    );
    assert_eq!(
        replay.stdout, first.stdout,
        "terminal op-id replay should preserve stdout exactly"
    );
    assert_eq!(
        replay.stderr, first.stderr,
        "terminal op-id replay should preserve stderr exactly"
    );

    let pending_op_id = "550e8400-e29b-41d4-a716-446655440003";
    let parsed_pending_op_id = pending_op_id.parse().expect("valid op id");
    let repo = Repository::open(temp.path()).expect("repo should open");
    let store = OperationDedupStore::open(repo.heddle_dir()).expect("open op-id store");
    let request_hash = hash_request_body(b"--output\0json\0thread\0drop\0pending-thread");
    let reserved = store
        .reserve(parsed_pending_op_id, "thread drop", request_hash)
        .expect("reserve pending op-id");
    assert!(
        matches!(reserved, repo::operation_dedup::DedupOutcome::Reserved),
        "test setup should reserve a fresh op-id slot"
    );

    let in_flight = heddle_output(
        &[
            "--output",
            "json",
            "--op-id",
            pending_op_id,
            "thread",
            "drop",
            "pending-thread",
        ],
        Some(temp.path()),
    )
    .expect("invoke in-flight op-id");
    assert!(
        !in_flight.status.success(),
        "in-flight op-id should fail closed"
    );
    assert!(
        in_flight.stdout.is_empty(),
        "op-id in-flight refusal should keep stdout quiet: {}",
        String::from_utf8_lossy(&in_flight.stdout)
    );
    let stderr = std::str::from_utf8(&in_flight.stderr).unwrap();
    let envelope: Value = serde_json::from_str(stderr)
        .unwrap_or_else(|err| panic!("in-flight refusal should be JSON: {err}: {stderr}"));
    assert_eq!(envelope["kind"], "op_id_in_flight");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("currently being executed")
                && error.contains("Unsafe:")
                && error.contains("Would change:")
                && error.contains("Preserved:")
                && error.contains("Primary recovery:")),
        "op-id in-flight refusal should use typed recovery detail: {stderr}"
    );
}

#[test]
fn attempt_invalid_count_uses_typed_advice_json() {
    let output = heddle_output(&["--output", "json", "attempt", "0", "--", "true"], None)
        .expect("invoke invalid attempt count");
    assert!(!output.status.success(), "attempt 0 should fail");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode attempt refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("attempt count refusal should emit JSON envelope");
    assert_eq!(envelope["kind"], "attempt_count_invalid");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("N must be at least 1")
                && error.contains("Unsafe:")
                && error.contains("Would change:")
                && error.contains("Preserved:")
                && error.contains("Primary recovery:")),
        "attempt count refusal should include typed recovery detail: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("attempt 1")),
        "attempt count hint should name a valid retry: {stderr}"
    );
}

#[test]
fn watch_empty_since_uses_typed_advice_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "watch", "--since", ""],
        Some(temp.path()),
    )
    .expect("invoke watch with empty since");
    assert!(!output.status.success(), "watch --since '' should fail");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode watch refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("watch since refusal should emit JSON envelope");
    assert_eq!(envelope["kind"], "watch_since_empty");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("--since cannot be empty")
                && error.contains("Unsafe:")
                && error.contains("Would change:")
                && error.contains("Preserved:")
                && error.contains("Primary recovery:")),
        "watch since refusal should include typed recovery detail: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("30s") && hint.contains("5m")),
        "watch since hint should name valid durations: {stderr}"
    );
}

#[test]
fn core_loop_schemas_are_discoverable() {
    for verb in [
        "capture",
        "commit",
        "checkpoint",
        "diff",
        "remote list",
        "remote show",
        "fetch",
        "pull",
        "push",
    ] {
        let mut args = vec!["schemas"];
        args.extend(verb.split_whitespace());
        let json = heddle(&args, None).unwrap_or_else(|err| panic!("schema for {verb}: {err}"));
        let parsed: Value = serde_json::from_str(&json)
            .unwrap_or_else(|err| panic!("schema for {verb} should parse: {err}: {json}"));
        assert!(
            parsed.get("title").is_some(),
            "schema for {verb} should have a title: {parsed}"
        );
    }
}

#[test]
fn core_git_overlay_json_surfaces_emit_one_machine_value() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(
        &["bridge", "git", "import", "--ref", "main"],
        Some(temp.path()),
    )
    .unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "tracked changed\n").unwrap();
    json_value(temp.path(), &["--json", "commit", "-m", "checkpoint"]);

    for (label, args) in [
        ("commands", vec!["commands", "--output", "json"]),
        ("schemas status", vec!["schemas", "status"]),
        ("status", vec!["status", "--json"]),
        ("diagnose", vec!["diagnose", "--json"]),
        ("doctor", vec!["doctor", "--output", "json"]),
        ("trust", vec!["trust", "--json"]),
        (
            "bridge git status",
            vec!["bridge", "git", "status", "--json"],
        ),
        ("log", vec!["log", "--json"]),
        ("show", vec!["show", "HEAD", "--json"]),
        ("thread list", vec!["thread", "list", "--json"]),
        ("thread show", vec!["thread", "show", "main", "--json"]),
        ("workspace show", vec!["workspace", "show", "--json"]),
        ("diff", vec!["diff", "--json"]),
        ("ready", vec!["ready", "--json"]),
    ] {
        let output = heddle_output(&args, Some(temp.path()))
            .unwrap_or_else(|err| panic!("invoke {label}: {err}"));
        assert!(
            output.status.success(),
            "{label} should succeed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "{label} JSON success should keep stderr quiet: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = std::str::from_utf8(&output.stdout).expect("stdout should be utf8");
        let parsed = parse_exactly_one_json_value(stdout)
            .unwrap_or_else(|err| panic!("{label} should emit one JSON value: {err}: {stdout}"));
        assert!(
            parsed.is_object(),
            "{label} should emit a JSON object machine contract: {parsed}"
        );
    }
}

#[test]
fn emitted_first_run_recommended_actions_parse_through_clap() {
    let catalog = parse_exactly_one_json_value(
        &heddle(&["commands", "--output", "json"], None).expect("commands JSON"),
    )
    .expect("commands should emit one JSON value");
    let placeholders = catalog["recommended_action_placeholders"]
        .as_array()
        .expect("catalog should expose placeholders")
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<std::collections::BTreeSet<_>>();

    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");

    for args in [
        vec!["status", "--json"],
        vec!["diagnose", "--json"],
        vec!["trust", "--json"],
        vec!["bridge", "git", "status", "--json"],
        vec!["thread", "list", "--json"],
        vec!["thread", "show", "main", "--json"],
        vec!["workspace", "show", "--json"],
    ] {
        let value = json_value(temp.path(), &args);
        assert_runtime_actions_parse(&value, &placeholders, &args);
    }

    heddle(&["init"], Some(temp.path())).unwrap();
    for args in [
        vec!["status", "--json"],
        vec!["diagnose", "--json"],
        vec!["trust", "--json"],
        vec!["bridge", "git", "status", "--json"],
        vec!["thread", "list", "--json"],
        vec!["thread", "show", "main", "--json"],
        vec!["workspace", "show", "--json"],
    ] {
        let value = json_value(temp.path(), &args);
        assert_runtime_actions_parse(&value, &placeholders, &args);
    }
}

fn json_value(cwd: &std::path::Path, args: &[&str]) -> Value {
    let output = heddle(args, Some(cwd)).unwrap_or_else(|err| panic!("heddle {args:?}: {err}"));
    parse_exactly_one_json_value(&output)
        .unwrap_or_else(|err| panic!("heddle {args:?} should emit one JSON value: {err}: {output}"))
}

fn assert_runtime_actions_parse(
    value: &Value,
    placeholders: &std::collections::BTreeSet<String>,
    source_args: &[&str],
) {
    let mut actions = Vec::new();
    collect_runtime_actions(value, &mut actions);
    assert!(
        !actions.is_empty(),
        "{source_args:?} should expose at least one machine action field: {value}"
    );
    for action in actions {
        let trimmed = action.trim();
        if trimmed.is_empty() || placeholders.contains(trimmed) {
            continue;
        }
        let argv = split_recommended_action_for_test(trimmed)
            .unwrap_or_else(|err| panic!("{source_args:?} action should split: {err}: {trimmed}"));
        assert_eq!(
            argv.first().map(String::as_str),
            Some("heddle"),
            "{source_args:?} action should use heddle or a registered placeholder: {trimmed}"
        );
        Cli::command()
            .try_get_matches_from(argv.clone())
            .unwrap_or_else(|err| {
                panic!(
                    "{source_args:?} action should parse through clap: {err}: {}",
                    argv.join(" ")
                )
            });
    }
}

fn collect_runtime_actions(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                match (key.as_str(), value) {
                    ("recommended_action" | "next_action", Value::String(action)) => {
                        out.push(action.clone());
                    }
                    ("recovery_commands", Value::Array(commands)) => {
                        out.extend(
                            commands
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string),
                        );
                    }
                    _ => collect_runtime_actions(value, out),
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_runtime_actions(value, out);
            }
        }
        _ => {}
    }
}

fn split_recommended_action_for_test(action: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut chars = action.chars().peekable();
    let mut in_double_quote = false;

    while let Some(ch) = chars.next() {
        match ch {
            '"' => in_double_quote = !in_double_quote,
            '\\' if in_double_quote => match chars.next() {
                Some(next) => current.push(next),
                None => current.push('\\'),
            },
            ch if ch.is_whitespace() && !in_double_quote => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            ch => current.push(ch),
        }
    }

    if in_double_quote {
        return Err("unterminated double quote".to_string());
    }
    if !current.is_empty() {
        args.push(current);
    }
    Ok(args)
}

fn init_git_repo_for_json_contract(path: &std::path::Path, branch: &str) {
    let status = std::process::Command::new("git")
        .args(["init", "--initial-branch", branch])
        .current_dir(path)
        .status()
        .expect("git init should run");
    assert!(status.success(), "git init should succeed");
    for (key, value) in [
        ("user.name", "Heddle Test"),
        ("user.email", "heddle@example.com"),
    ] {
        let status = std::process::Command::new("git")
            .args(["config", key, value])
            .current_dir(path)
            .status()
            .expect("git config should run");
        assert!(status.success(), "git config {key} should succeed");
    }
}

fn git_commit_all_for_json_contract(path: &std::path::Path, message: &str) {
    for args in [&["add", "."][..], &["commit", "-m", message][..]] {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(path)
            .status()
            .expect("git should run");
        assert!(status.success(), "git {args:?} should succeed");
    }
}

fn parse_exactly_one_json_value(raw: &str) -> Result<Value, String> {
    let mut values = serde_json::Deserializer::from_str(raw).into_iter::<Value>();
    let value = values
        .next()
        .ok_or_else(|| "stdout was empty".to_string())?
        .map_err(|err| err.to_string())?;
    match values.next() {
        Some(Ok(extra)) => Err(format!("extra JSON value after first value: {extra}")),
        Some(Err(err)) => Err(err.to_string()),
        None => Ok(value),
    }
}

#[test]
fn git_compat_commit_branch_and_switch_shims_work() {
    let temp = TempDir::new().unwrap();
    gix::init(temp.path()).expect("init git repo");
    std::fs::write(temp.path().join("seed.txt"), "seed\n").unwrap();

    let commit_json = heddle(
        &["--output", "json", "commit", "-m", "seed commit"],
        Some(temp.path()),
    )
    .unwrap();
    let commit: Value = serde_json::from_str(&commit_json).unwrap();
    assert_eq!(commit["action"], "commit");
    assert!(
        commit["change_id"]
            .as_str()
            .unwrap_or("")
            .starts_with("hd-")
    );
    assert!(
        commit["git_commit"].as_str().unwrap_or("").len() >= 7,
        "commit shim should write a Git checkpoint: {commit}"
    );

    let branch = heddle(&["branch", "feature/git-shim"], Some(temp.path())).unwrap();
    assert!(
        branch.contains("feature/git-shim") || branch.contains("Created"),
        "branch shim should create a thread: {branch}"
    );

    let switched = heddle(&["switch", "feature/git-shim"], Some(temp.path())).unwrap();
    assert!(
        switched.contains("feature/git-shim") || switched.contains("Switched"),
        "switch shim should route to thread switch: {switched}"
    );
}

#[test]
fn thread_switch_refuses_dirty_worktree_without_force() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();
    heddle(
        &["thread", "create", "feature/dirty-switch"],
        Some(temp.path()),
    )
    .unwrap();

    std::fs::write(temp.path().join("tracked.txt"), "dirty\n").unwrap();
    let output = heddle_output(
        &["thread", "switch", "feature/dirty-switch"],
        Some(temp.path()),
    )
    .expect("invoke dirty switch");
    assert!(!output.status.success(), "dirty switch should fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Refusing to switch threads") && stderr.contains("heddle stash push"),
        "dirty switch should name preservation commands: {stderr}"
    );

    let forced = heddle(
        &["thread", "switch", "feature/dirty-switch", "--force"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        forced.contains("feature/dirty-switch"),
        "forced switch should still be explicit about target: {forced}"
    );
}

#[test]
fn remote_list_and_show_json_share_git_overlay_remote_view() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    gix::init_bare(&origin).expect("init bare origin");
    gix::init(temp.path()).expect("init git worktree");
    std::fs::OpenOptions::new()
        .append(true)
        .open(temp.path().join(".git/config"))
        .unwrap()
        .write_all(
            format!(
                "\n[remote \"origin\"]\n\turl = {}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
                origin.display()
            )
            .as_bytes(),
        )
        .unwrap();

    let list_json = heddle(&["--output", "json", "remote", "list"], Some(temp.path())).unwrap();
    let list: Value = serde_json::from_str(&list_json).unwrap();
    let remotes = list["remotes"].as_array().unwrap();
    assert!(
        remotes.iter().any(|remote| remote["name"] == "origin"
            && remote["source"] == "git-overlay"
            && remote["is_default"] == true),
        "remote list should include Git-overlay origin: {list}"
    );

    let show_json = heddle(
        &["--output", "json", "remote", "show", "origin"],
        Some(temp.path()),
    )
    .unwrap();
    let show: Value = serde_json::from_str(&show_json).unwrap();
    assert_eq!(show["name"], "origin");
    assert_eq!(show["source"], "git-overlay");
    assert_eq!(show["is_default"], true);
}

#[test]
fn branch_delete_current_refuses_with_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "branch", "-d", "main"],
        Some(temp.path()),
    )
    .expect("invoke current branch delete");
    assert!(
        !output.status.success(),
        "deleting the current branch should fail"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("current branch delete should emit JSON envelope");
    assert_eq!(envelope["kind"], "branch_delete_current");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("Refusing to delete current thread")),
        "error should explain the unsafe branch delete: {envelope}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle thread list")),
        "hint should name the primary recovery command: {envelope}"
    );
}

#[test]
fn empty_undo_redo_refuse_with_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for (args, kind, label) in [
        (
            ["--output", "json", "undo"],
            "nothing_to_undo",
            "Nothing to undo",
        ),
        (
            ["--output", "json", "redo"],
            "nothing_to_redo",
            "Nothing to redo",
        ),
    ] {
        let output = heddle_output(&args, Some(temp.path())).expect("invoke undo/redo");
        assert!(!output.status.success(), "{label} should fail");
        let stderr = std::str::from_utf8(&output.stderr).unwrap();
        let envelope: Value =
            serde_json::from_str(stderr).expect("empty undo/redo should emit JSON envelope");
        assert_eq!(envelope["kind"], kind);
        assert!(
            envelope["error"]
                .as_str()
                .is_some_and(|error| error.contains(label)),
            "error should name the empty history: {envelope}"
        );
        assert!(
            envelope["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("heddle undo --list")),
            "hint should name the inspection command: {envelope}"
        );
    }
}

#[test]
fn undo_list_preview_conflict_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "undo", "--list", "--preview"],
        Some(temp.path()),
    )
    .expect("invoke undo mode conflict");
    assert!(
        !output.status.success(),
        "undo --list --preview should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode undo mode refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("undo mode conflict should emit JSON envelope");
    assert_eq!(envelope["kind"], "undo_mode_conflict");
    assert!(
        envelope["error"].as_str().is_some_and(|error| error
            .contains("Use either --list or --preview")
            && error.contains("Unsafe:")
            && error.contains("Would change:")
            && error.contains("Preserved:")
            && error.contains("Primary recovery:")),
        "undo mode conflict should include full typed advice: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle undo --list")
                && hint.contains("heddle undo --preview")),
        "undo mode conflict hint should name both valid commands: {stderr}"
    );
}

#[test]
fn empty_stash_refusals_use_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for (args, kind, label, recovery) in [
        (
            ["--output", "json", "stash", "push"],
            "no_changes_to_stash",
            "No changes to stash",
            "heddle status",
        ),
        (
            ["--output", "json", "stash", "drop"],
            "no_stash_available",
            "No stash to drop",
            "heddle stash list",
        ),
        (
            ["--output", "json", "stash", "apply"],
            "no_stash_available",
            "No stash found",
            "heddle stash list",
        ),
    ] {
        let output = heddle_output(&args, Some(temp.path())).expect("invoke stash refusal");
        assert!(!output.status.success(), "{label} should fail");
        assert!(
            output.stdout.is_empty(),
            "JSON failure must keep stdout quiet: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        let stderr = std::str::from_utf8(&output.stderr).unwrap();
        let envelope: Value =
            serde_json::from_str(stderr).expect("stash refusal should emit JSON envelope");
        assert_eq!(envelope["kind"], kind);
        assert!(
            envelope["error"]
                .as_str()
                .is_some_and(|error| error.contains(label)
                    && error.contains("Unsafe:")
                    && error.contains("Would change:")
                    && error.contains("Preserved:")
                    && error.contains("Primary recovery:")),
            "error should keep the full typed advice: {envelope}"
        );
        assert!(
            envelope["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains(recovery)),
            "hint should name the primary recovery command: {envelope}"
        );
    }
}

#[test]
fn undo_thread_create_with_live_worktree_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    let worktree = temp.path().join("feature-wt");
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();
    heddle(
        &[
            "start",
            "feature",
            "--path",
            worktree.to_str().unwrap(),
            "--workspace",
            "solid",
        ],
        Some(temp.path()),
    )
    .unwrap();

    let output =
        heddle_output(&["--output", "json", "undo"], Some(temp.path())).expect("invoke undo");
    assert!(
        !output.status.success(),
        "undo of start --path should refuse while the worktree exists"
    );
    assert!(
        worktree.exists(),
        "typed refusal must not remove the materialized worktree"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("live worktree undo should emit JSON envelope");
    assert_eq!(envelope["kind"], "thread_worktree_undo_unsafe");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("orphaned by the inverse")),
        "error should explain the unsafe undo: {envelope}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle thread drop feature --delete-thread")),
        "hint should name the exact teardown command: {envelope}"
    );
}

#[test]
fn rebase_continue_abort_without_operation_use_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for args in [
        ["--output", "json", "rebase", "--continue"],
        ["--output", "json", "rebase", "--abort"],
    ] {
        let output = heddle_output(&args, Some(temp.path())).expect("invoke rebase recovery");
        assert!(
            !output.status.success(),
            "rebase recovery without an operation should fail"
        );
        let stderr = std::str::from_utf8(&output.stderr).unwrap();
        let envelope: Value =
            serde_json::from_str(stderr).expect("rebase recovery should emit JSON envelope");
        assert_eq!(envelope["kind"], "no_rebase_in_progress");
        assert!(
            envelope["error"]
                .as_str()
                .is_some_and(|error| error.contains("No rebase in progress")),
            "error should name the missing operation: {envelope}"
        );
        assert!(
            envelope["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("heddle status")),
            "hint should name the operation inspection command: {envelope}"
        );
    }
}

#[test]
fn rebase_target_refusals_use_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();

    for (args, kind, expected) in [
        (
            vec!["--output", "json", "rebase"],
            "rebase_target_required",
            "target thread required",
        ),
        (
            vec!["--output", "json", "rebase", "missing-thread"],
            "rebase_target_not_found",
            "missing-thread",
        ),
    ] {
        let output = heddle_output(&args, Some(temp.path())).expect("invoke rebase target refusal");
        assert!(
            !output.status.success(),
            "rebase target refusal should fail"
        );
        assert!(
            output.stdout.is_empty(),
            "JSON-mode rebase target refusal must keep stdout quiet: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        let stderr = std::str::from_utf8(&output.stderr).unwrap();
        let envelope: Value =
            serde_json::from_str(stderr).expect("rebase target refusal should emit JSON envelope");
        assert_eq!(envelope["kind"], kind);
        assert!(
            envelope["error"]
                .as_str()
                .is_some_and(|error| error.contains(expected)
                    && error.contains("Unsafe:")
                    && error.contains("Would change:")
                    && error.contains("Preserved:")
                    && error.contains("Primary recovery:")),
            "rebase target refusal should include full typed advice: {stderr}"
        );
        assert!(
            envelope["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("heddle thread list")),
            "rebase target hint should name thread inspection: {stderr}"
        );
    }
}

#[test]
fn cherry_pick_missing_commit_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "cherry-pick", "hd-deadbeef1234"],
        Some(temp.path()),
    )
    .expect("invoke cherry-pick target refusal");
    assert!(
        !output.status.success(),
        "missing cherry-pick commit should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode cherry-pick refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("cherry-pick refusal should emit JSON envelope");
    assert_eq!(envelope["kind"], "cherry_pick_commit_not_found");
    assert!(
        envelope["error"].as_str().is_some_and(|error| error
            .contains("commit 'hd-deadbeef1234' not found")
            && error.contains("Unsafe:")
            && error.contains("Would change:")
            && error.contains("Preserved:")
            && error.contains("Primary recovery:")),
        "cherry-pick refusal should include full typed advice: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle log")),
        "cherry-pick hint should name history inspection: {stderr}"
    );
}

#[test]
fn goto_missing_state_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "goto", "hd-deadbeef1234"],
        Some(temp.path()),
    )
    .expect("invoke goto target refusal");
    assert!(!output.status.success(), "missing goto target should fail");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode goto refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("goto missing state should emit JSON envelope");
    assert_eq!(envelope["kind"], "state_not_found");
    assert!(
        envelope["error"].as_str().is_some_and(|error| error
            .contains("State not found: hd-deadbeef1234")
            && error.contains("Unsafe:")
            && error.contains("Would change:")
            && error.contains("Preserved:")
            && error.contains("Primary recovery:")),
        "goto missing state should include full typed advice: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle log")),
        "goto missing state hint should name history inspection: {stderr}"
    );
}

#[test]
fn bisect_good_bad_without_operation_use_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for args in [
        ["--output", "json", "bisect", "good", "HEAD"],
        ["--output", "json", "bisect", "bad", "HEAD"],
    ] {
        let output = heddle_output(&args, Some(temp.path())).expect("invoke bisect mark");
        assert!(
            !output.status.success(),
            "bisect mark without an operation should fail"
        );
        let stderr = std::str::from_utf8(&output.stderr).unwrap();
        let envelope: Value =
            serde_json::from_str(stderr).expect("bisect recovery should emit JSON envelope");
        assert_eq!(envelope["kind"], "no_bisect_in_progress");
        assert!(
            envelope["error"]
                .as_str()
                .is_some_and(|error| error.contains("No bisect in progress")),
            "error should name the missing operation: {envelope}"
        );
        assert!(
            envelope["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("heddle bisect start")),
            "hint should name the start command: {envelope}"
        );
    }
}

#[test]
fn thread_start_active_reservation_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();

    heddle(
        &[
            "start",
            "feature/reserved-json",
            "--workspace",
            "solid",
            "--path",
            first.to_str().unwrap(),
        ],
        Some(temp.path()),
    )
    .unwrap();

    let output = heddle_output(
        &[
            "--output",
            "json",
            "start",
            "feature/reserved-json",
            "--workspace",
            "solid",
            "--path",
            second.to_str().unwrap(),
        ],
        Some(temp.path()),
    )
    .expect("invoke second thread start");
    assert!(
        !output.status.success(),
        "second active writer should be rejected"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("reservation conflict should emit JSON envelope");
    assert_eq!(envelope["kind"], "active_thread_reservation");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("already has an active reservation")),
        "error should name the active reservation: {envelope}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle thread show feature/reserved-json")),
        "hint should name the inspection command: {envelope}"
    );
}

#[test]
fn thread_start_anchor_mismatch_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    let checkout = temp.path().join("feature-checkout");
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();
    heddle(&["thread", "create", "feature/anchored"], Some(temp.path())).unwrap();

    std::fs::write(temp.path().join("tracked.txt"), "next\n").unwrap();
    heddle(&["capture", "-m", "next"], Some(temp.path())).unwrap();
    let requested = Repository::open(temp.path())
        .unwrap()
        .current_state()
        .unwrap()
        .unwrap()
        .change_id
        .to_string();

    let output = heddle_output(
        &[
            "--output",
            "json",
            "start",
            "feature/anchored",
            "--from",
            &requested,
            "--path",
            checkout.to_str().unwrap(),
        ],
        Some(temp.path()),
    )
    .expect("invoke thread start with mismatched anchor");
    assert!(
        !output.status.success(),
        "thread start with mismatched --from should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode anchor mismatch must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("anchor mismatch should emit JSON envelope");
    assert_eq!(envelope["kind"], "thread_anchor_mismatch");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("feature/anchored")
                && error.contains("--from resolved")
                && error.contains("Unsafe:")
                && error.contains("Would change:")
                && error.contains("Preserved:")
                && error.contains("Primary recovery:")),
        "anchor mismatch should keep full typed advice: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle thread show feature/anchored")),
        "anchor mismatch should name the inspection command: {stderr}"
    );
}

#[test]
fn thread_switch_from_worktree_to_shared_thread_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    let alpha = temp.path().join("alpha-worktree");
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();
    heddle(
        &[
            "start",
            "alpha/worktree",
            "--workspace",
            "solid",
            "--path",
            alpha.to_str().unwrap(),
        ],
        Some(temp.path()),
    )
    .unwrap();
    heddle(&["thread", "create", "beta/shared"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "thread", "switch", "beta/shared"],
        Some(&alpha),
    )
    .expect("invoke thread switch from dedicated worktree");
    assert!(
        !output.status.success(),
        "switching to shared thread from dedicated worktree should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode switch refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("switch refusal should emit JSON envelope");
    assert_eq!(envelope["kind"], "thread_switch_would_overwrite_worktree");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("beta/shared")
                && error.contains("no dedicated worktree")
                && error.contains("Unsafe:")
                && error.contains("Would change:")
                && error.contains("Preserved:")
                && error.contains("Primary recovery:")),
        "switch refusal should keep full typed advice: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle start --workspace materialized beta/shared")),
        "switch refusal should name the materialization command: {stderr}"
    );
}

#[test]
fn dirty_goto_start_path_and_drop_refuse_without_force() {
    let temp = TempDir::new().unwrap();
    let checkout = temp.path().join("worker");
    let checkout_arg = checkout.to_str().unwrap();

    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();
    let base = Repository::open(temp.path())
        .unwrap()
        .current_state()
        .unwrap()
        .unwrap()
        .change_id
        .to_string();
    std::fs::write(temp.path().join("tracked.txt"), "next\n").unwrap();
    heddle(&["capture", "-m", "next"], Some(temp.path())).unwrap();

    std::fs::write(temp.path().join("tracked.txt"), "dirty\n").unwrap();
    let goto = heddle_output(&["goto", &base], Some(temp.path())).expect("invoke goto");
    assert!(!goto.status.success(), "dirty goto should fail");
    let stderr = String::from_utf8_lossy(&goto.stderr);
    assert!(
        stderr.contains("Refusing to goto") && stderr.contains("heddle capture"),
        "dirty goto should use the shared preservation hint: {stderr}"
    );

    let start = heddle_output(
        &["start", "dirty-start", "--path", checkout_arg],
        Some(temp.path()),
    )
    .expect("invoke start");
    assert!(!start.status.success(), "dirty start --path should fail");
    let stderr = String::from_utf8_lossy(&start.stderr);
    assert!(
        stderr.contains("Refusing to start thread") && stderr.contains("heddle stash push"),
        "dirty start --path should use the shared preservation hint: {stderr}"
    );

    heddle(&["goto", &base, "--force"], Some(temp.path())).unwrap();
    heddle(
        &["start", "drop-target", "--path", checkout_arg],
        Some(temp.path()),
    )
    .unwrap();
    std::fs::write(checkout.join("tracked.txt"), "dirty worker\n").unwrap();
    let drop = heddle_output(&["thread", "drop", "drop-target"], Some(temp.path()))
        .expect("invoke thread drop");
    assert!(!drop.status.success(), "dirty drop should fail");
    let stderr = String::from_utf8_lossy(&drop.stderr);
    assert!(
        stderr.contains("Refusing to drop thread") && stderr.contains("heddle capture"),
        "dirty drop should use the shared preservation hint: {stderr}"
    );
    let forced = heddle(
        &["thread", "drop", "drop-target", "--force"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        forced.contains("drop-target"),
        "forced drop should still name the target: {forced}"
    );
}

#[test]
fn revert_refuses_dirty_worktree_with_shared_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "next\n").unwrap();
    heddle(&["capture", "-m", "next"], Some(temp.path())).unwrap();
    let target = Repository::open(temp.path())
        .unwrap()
        .current_state()
        .unwrap()
        .unwrap()
        .change_id
        .to_string();

    std::fs::write(temp.path().join("tracked.txt"), "dirty\n").unwrap();
    let output = heddle_output(&["revert", &target], Some(temp.path())).expect("invoke revert");
    assert!(!output.status.success(), "dirty revert should fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: Value =
        serde_json::from_str(&stderr).expect("dirty revert should emit JSON error envelope");
    assert!(
        envelope["kind"] == "dirty_worktree"
            && envelope["error"]
                .as_str()
                .is_some_and(|error| error.contains("Refusing to revert")
                    && error.contains("tracked.txt")
                    && error.contains("heddle capture -m \"...\"")
                    && error.contains("heddle stash push -m \"...\""))
            && envelope["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("heddle capture -m \"...\"")
                    && hint.contains("heddle stash push -m \"...\"")),
        "dirty revert should use the shared typed preservation advice: {stderr}"
    );
}

#[test]
fn revert_empty_state_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(&["capture", "-m", "empty"], Some(temp.path())).unwrap();
    let target = Repository::open(temp.path())
        .unwrap()
        .current_state()
        .unwrap()
        .unwrap()
        .change_id
        .to_string();

    let output = heddle_output(&["--output", "json", "revert", &target], Some(temp.path()))
        .expect("invoke empty revert");
    assert!(!output.status.success(), "empty revert should fail");
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("empty revert should emit JSON envelope");
    assert_eq!(envelope["kind"], "no_changes_to_revert");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("No changes to revert")),
        "error should name the empty diff: {envelope}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle show")),
        "hint should name the inspection command: {envelope}"
    );
}

#[test]
fn checkpoint_refuses_uncaptured_worktree_with_shared_advice() {
    let temp = TempDir::new().unwrap();
    gix::init(temp.path()).expect("init git repo");
    std::fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
    heddle(
        &["--output", "json", "commit", "-m", "seed checkpoint"],
        Some(temp.path()),
    )
    .unwrap();

    std::fs::write(temp.path().join("tracked.txt"), "dirty\n").unwrap();
    std::fs::write(temp.path().join("scratch.txt"), "new\n").unwrap();
    let output = heddle_output(
        &["--output", "json", "checkpoint", "-m", "blocked checkpoint"],
        Some(temp.path()),
    )
    .expect("invoke checkpoint");
    assert!(!output.status.success(), "dirty checkpoint should fail");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode checkpoint refusal must not write stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: Value =
        serde_json::from_str(&stderr).expect("dirty checkpoint should emit JSON error envelope");
    assert!(
        envelope["kind"] == "dirty_worktree"
            && envelope["error"].as_str().is_some_and(|error| error
                .contains("Refusing to checkpoint")
                && error.contains("tracked.txt")
                && error.contains("scratch.txt")
                && error.contains("Heddle state was left unchanged")
                && error.contains("heddle capture -m \"...\"")
                && error.contains("heddle stash push -m \"...\""))
            && envelope["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("heddle capture -m \"...\"")
                    && hint.contains("heddle stash push -m \"...\"")),
        "dirty checkpoint should use the shared typed preservation advice: {stderr}"
    );
}

#[test]
fn clean_refuses_without_force_with_shared_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("scratch.txt"), "new\n").unwrap();

    let output =
        heddle_output(&["--output", "json", "clean"], Some(temp.path())).expect("invoke clean");
    assert!(!output.status.success(), "clean without force should fail");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode clean refusal must not write stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: Value =
        serde_json::from_str(&stderr).expect("clean refusal should emit JSON error envelope");
    assert!(
        envelope["kind"] == "destructive_requires_force"
            && envelope["error"]
                .as_str()
                .is_some_and(|error| error.contains("Refusing to clean")
                    && error.contains("destructive action requires --force")
                    && error.contains("untracked paths")
                    && error.contains("nothing was removed")
                    && error.contains("heddle clean --dry-run"))
            && envelope["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("heddle clean --dry-run")
                    && hint.contains("heddle clean --force")),
        "clean refusal should use the shared typed force advice: {stderr}"
    );
}

#[test]
fn clone_existing_destination_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    let existing = temp.path().join("existing");
    std::fs::create_dir(&existing).unwrap();

    let output = heddle_output(
        &[
            "--output",
            "json",
            "clone",
            "not-a-real-remote",
            existing.to_str().unwrap(),
        ],
        Some(temp.path()),
    )
    .expect("invoke clone refusal");
    assert!(
        !output.status.success(),
        "clone into existing destination should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode clone refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("clone refusal should emit JSON envelope");
    assert_eq!(envelope["kind"], "clone_destination_exists");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("already exists")
                && error.contains("Unsafe:")
                && error.contains("Would change:")
                && error.contains("Preserved:")
                && error.contains("Primary recovery:")),
        "clone destination refusal should keep full typed advice: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle clone")),
        "clone destination refusal should name the recovery command: {stderr}"
    );
}

#[test]
fn clone_invalid_remote_url_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("target");

    let output = heddle_output(
        &[
            "--output",
            "json",
            "clone",
            "::not-a-valid-remote::",
            target.to_str().unwrap(),
        ],
        Some(temp.path()),
    )
    .expect("invoke clone refusal");
    assert!(
        !output.status.success(),
        "clone with invalid remote should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode invalid clone refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !target.exists(),
        "invalid remote rejection must run before destination creation"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("invalid clone remote should emit JSON envelope");
    assert_eq!(envelope["kind"], "clone_invalid_remote_url");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("Invalid remote URL")
                && error.contains("Unsafe:")
                && error.contains("Would change:")
                && error.contains("Preserved:")
                && error.contains("Primary recovery:")),
        "clone invalid remote refusal should keep full typed advice: {stderr}"
    );
    assert!(
        envelope["hint"].as_str().is_some_and(
            |hint| hint.contains("file:///path/to/repo") && hint.contains("Git clone URL")
        ),
        "clone invalid remote hint should name valid remote shapes: {stderr}"
    );
}

#[test]
fn clone_missing_remote_thread_uses_typed_advice_without_destination_side_effects() {
    let temp = TempDir::new().unwrap();
    let remote = temp.path().join("remote");
    let target = temp.path().join("target");
    std::fs::create_dir(&remote).unwrap();
    heddle(&["init"], Some(&remote)).unwrap();
    std::fs::write(remote.join("base.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(&remote)).unwrap();

    let output = heddle_output(
        &[
            "--output",
            "json",
            "clone",
            remote.to_str().unwrap(),
            target.to_str().unwrap(),
            "--thread",
            "missing-thread",
        ],
        Some(temp.path()),
    )
    .expect("invoke clone missing thread refusal");
    assert!(
        !output.status.success(),
        "clone with missing remote thread should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode clone missing-thread refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !target.exists(),
        "missing thread refusal must run before destination initialization"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("clone missing thread should emit JSON envelope");
    assert_eq!(envelope["kind"], "clone_remote_thread_not_found");
    assert!(
        envelope["error"].as_str().is_some_and(|error| error
            .contains("Thread 'missing-thread' not found in remote")
            && error.contains("Unsafe:")
            && error.contains("Would change:")
            && error.contains("Preserved:")
            && error.contains("Primary recovery:")),
        "clone missing thread refusal should keep full typed advice: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle thread list")),
        "clone missing thread hint should name thread inspection: {stderr}"
    );
}

#[test]
fn thread_drop_missing_thread_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "thread", "drop", "missing-thread"],
        Some(temp.path()),
    )
    .expect("invoke missing thread drop");
    assert!(!output.status.success(), "missing thread drop should fail");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode missing thread drop refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("missing thread drop should emit JSON envelope");
    assert_eq!(envelope["kind"], "thread_not_found");
    assert!(
        envelope["error"].as_str().is_some_and(|error| error
            .contains("Thread 'missing-thread' not found")
            && error.contains("Unsafe:")
            && error.contains("Would change:")
            && error.contains("Preserved:")
            && error.contains("Primary recovery:")),
        "missing thread drop should include typed recovery detail: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle thread list")),
        "missing thread drop hint should name thread list: {stderr}"
    );
}

#[test]
fn thread_switch_missing_thread_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "thread", "switch", "missing-thread"],
        Some(temp.path()),
    )
    .expect("invoke missing thread switch");
    assert!(
        !output.status.success(),
        "missing thread switch should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode missing thread switch refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("missing thread switch should emit JSON envelope");
    assert_eq!(envelope["kind"], "thread_not_found");
    assert!(
        envelope["error"].as_str().is_some_and(|error| error
            .contains("Thread 'missing-thread' not found")
            && error.contains("Unsafe:")
            && error.contains("Would change:")
            && error.contains("Preserved:")
            && error.contains("Primary recovery:")),
        "missing thread switch should include typed recovery detail: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle thread list")),
        "missing thread switch hint should name thread list: {stderr}"
    );
}

#[test]
fn doctor_uses_recovery_language_without_breaking_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("work.txt"), "pending").unwrap();

    let text = heddle(&["--output", "text", "doctor"], Some(temp.path())).unwrap();
    assert!(
        text.contains("Doctor"),
        "doctor should render a human header: {text}"
    );
    assert!(
        text.contains("Health: uncaptured"),
        "doctor should label the freshly-initialized worktree as uncaptured: {text}"
    );
    assert!(
        text.contains("Next step: heddle capture"),
        "doctor should provide one primary recovery command: {text}"
    );
    assert!(
        !text.contains("Next:"),
        "doctor should use the newer next-step label: {text}"
    );

    let json = heddle(&["doctor", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&json).expect("doctor JSON should parse");
    assert_eq!(parsed["health"]["recommended_action"], "heddle capture");
}

#[test]
fn profile_env_writes_timings_to_stderr_without_polluting_json_stdout() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output_with_env(
        &["--output", "json", "status"],
        Some(temp.path()),
        &[("HEDDLE_PROFILE", "1")],
    )
    .expect("status should run");
    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    let stderr = std::str::from_utf8(&output.stderr).unwrap();

    assert!(
        output.status.success(),
        "profiled status should succeed; stdout={stdout} stderr={stderr}"
    );
    serde_json::from_str::<Value>(stdout).expect("profiled JSON stdout should still parse");
    assert!(
        stderr.contains("heddle profile:"),
        "profile output should go to stderr: {stderr}"
    );
    assert!(
        stderr.contains("command: status phases"),
        "status should include command-specific phases: {stderr}"
    );
    assert!(
        stderr.contains("command: status worktree"),
        "status should include worktree-specific phases: {stderr}"
    );
    assert!(
        stderr.contains("worktree_status_ms:"),
        "status profile should show worktree scan cost: {stderr}"
    );
    assert!(
        stderr.contains("directories_scanned:"),
        "status profile should show worktree scan counters: {stderr}"
    );
    assert!(
        stderr.contains("command_body_ms:"),
        "top-level profile should show command body cost: {stderr}"
    );
}

#[test]
fn version_verbose_reports_bug_context() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let text = heddle(
        &["--output", "text", "version", "--verbose"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        text.contains("Heddle "),
        "version should identify Heddle: {text}"
    );
    assert!(
        text.contains("Build profile:"),
        "verbose version should show build profile: {text}"
    );
    assert!(
        text.contains("Git:"),
        "verbose version should show Git availability: {text}"
    );
    assert!(
        text.contains("Repository:"),
        "verbose version should show repository capability: {text}"
    );

    let json = heddle(&["version", "--verbose", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&json).expect("version JSON should parse");
    assert_eq!(parsed["version"], env!("CARGO_PKG_VERSION"));
    assert!(parsed["features"].as_array().is_some());
}

#[test]
fn start_merge_undo_json_workflow_keeps_machine_streams_clean() {
    fn json_success(args: &[&str], cwd: &std::path::Path) -> Value {
        let output = heddle_output(args, Some(cwd)).expect("invoke heddle");
        let stdout = std::str::from_utf8(&output.stdout).unwrap();
        let stderr = std::str::from_utf8(&output.stderr).unwrap();
        assert!(
            output.status.success(),
            "{args:?} should succeed; stdout={stdout} stderr={stderr}"
        );
        assert!(
            stderr.is_empty(),
            "{args:?} JSON success must keep stderr quiet: {stderr}"
        );
        serde_json::from_str(stdout)
            .unwrap_or_else(|_| panic!("{args:?} should emit parseable JSON: {stdout}"))
    }

    let temp = TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    let feature = temp.path().join("feature checkout");
    std::fs::create_dir_all(&repo).unwrap();

    json_success(&["--output", "json", "init"], &repo);
    std::fs::write(
        repo.join("app.txt"),
        "base
",
    )
    .unwrap();
    json_success(
        &[
            "--output",
            "json",
            "capture",
            "-m",
            "base",
            "--confidence",
            "0.9",
        ],
        &repo,
    );

    let started = json_success(
        &[
            "--output",
            "json",
            "start",
            "feature/a",
            "--path",
            feature.to_str().expect("utf8 path"),
            "--workspace",
            "solid",
        ],
        &repo,
    );
    assert_eq!(started["name"], "feature/a");
    assert_eq!(
        started["execution_path"].as_str(),
        Some(feature.to_str().expect("utf8 path"))
    );

    std::fs::write(
        feature.join("app.txt"),
        "base
feature
",
    )
    .unwrap();
    json_success(
        &[
            "--output",
            "json",
            "capture",
            "-m",
            "feature",
            "--confidence",
            "0.9",
        ],
        &feature,
    );

    let before_merge_preview = json_success(&["--output", "json", "status"], &repo);
    let preview = json_success(
        &["--output", "json", "merge", "feature/a", "--preview"],
        &repo,
    );
    assert_eq!(preview["status"], "preview");
    assert_eq!(preview["preview_only"], true);
    let after_merge_preview = json_success(&["--output", "json", "status"], &repo);
    assert_eq!(
        after_merge_preview["current_state"], before_merge_preview["current_state"],
        "merge --preview must not advance the current thread: before={before_merge_preview} after={after_merge_preview}"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("app.txt")).unwrap(),
        "base\n",
        "merge --preview must not modify the worktree"
    );

    let merged = json_success(&["--output", "json", "merge", "feature/a"], &repo);
    assert_eq!(merged["status"], "completed");
    assert_eq!(merged["fast_forward"], true);

    let before_repeat_merge = json_success(&["--output", "json", "status"], &repo);
    let repeat_merge = heddle_output(&["--output", "text", "merge", "feature/a"], Some(&repo))
        .expect("invoke repeat merge");
    assert!(
        repeat_merge.status.success(),
        "already-applied merge should be a successful no-op"
    );
    assert!(
        repeat_merge.stderr.is_empty(),
        "already-applied merge should keep stderr quiet: {}",
        String::from_utf8_lossy(&repeat_merge.stderr)
    );
    let repeat_stdout = String::from_utf8_lossy(&repeat_merge.stdout);
    assert!(
        repeat_stdout.contains("Already up to date"),
        "already-applied merge should name the no-op state: {repeat_stdout}"
    );
    let after_repeat_merge = json_success(&["--output", "json", "status"], &repo);
    assert_eq!(
        after_repeat_merge["current_state"], before_repeat_merge["current_state"],
        "already-applied merge must not advance state: before={before_repeat_merge} after={after_repeat_merge}"
    );

    let listed = json_success(&["--output", "json", "undo", "--list"], &repo);
    assert!(
        listed["batches"]
            .as_array()
            .is_some_and(|batches| !batches.is_empty()),
        "undo --list should expose recent operation batches: {listed}"
    );

    assert_eq!(
        std::fs::read_to_string(repo.join("app.txt")).unwrap(),
        "base\nfeature\n",
        "real merge should update the worktree before undo preview"
    );
    let before_undo_preview = json_success(&["--output", "json", "status"], &repo);
    let preview_undo = json_success(&["--output", "json", "undo", "--preview"], &repo);
    assert_eq!(preview_undo["action"], "undo");
    assert!(
        preview_undo["message"]
            .as_str()
            .unwrap_or("")
            .contains("Would undo"),
        "undo preview should clearly name the dry run: {preview_undo}"
    );
    let after_undo_preview = json_success(&["--output", "json", "status"], &repo);
    assert_eq!(
        after_undo_preview["current_state"], before_undo_preview["current_state"],
        "undo --preview must not advance or rewind the current thread: before={before_undo_preview} after={after_undo_preview}"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("app.txt")).unwrap(),
        "base\nfeature\n",
        "undo --preview must not modify the worktree"
    );
}

#[test]
fn version_verbose_honors_explicit_repo_path() {
    let temp = TempDir::new().unwrap();
    let repo = temp.path().join("explicit repo");
    std::fs::create_dir_all(&repo).unwrap();
    heddle(&["--repo", repo.to_str().expect("utf8 path"), "init"], None).unwrap();

    let json = heddle(
        &[
            "--repo",
            repo.to_str().expect("utf8 path"),
            "version",
            "--verbose",
            "--json",
        ],
        None,
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&json).expect("version JSON should parse");
    assert_eq!(parsed["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        parsed["repository_root"].as_str(),
        Some(repo.to_str().expect("utf8 path")),
        "version --repo should report the explicitly requested repository: {json}"
    );
}

#[test]
fn ready_text_names_ready_and_already_ready_noop_states() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("app.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();

    let first = heddle_output(&["--output", "text", "ready"], Some(temp.path()))
        .expect("invoke ready text");
    assert!(first.status.success(), "ready text should succeed");
    assert!(
        first.stderr.is_empty(),
        "ready text success should keep stderr quiet: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_stdout = String::from_utf8_lossy(&first.stdout);
    assert!(
        first_stdout.contains("no integration target") && first_stdout.contains("Readiness"),
        "ready text should name the clean no-target state: {first_stdout}"
    );
    assert!(
        !first_stdout.contains("heddle merge main"),
        "ready text must not recommend merging the current thread into itself: {first_stdout}"
    );

    let second = heddle_output(&["--output", "text", "ready"], Some(temp.path()))
        .expect("invoke ready text no-op");
    assert!(second.status.success(), "ready no-op should succeed");
    assert!(
        second.stderr.is_empty(),
        "ready no-op success should keep stderr quiet: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second_stdout = String::from_utf8_lossy(&second.stdout);
    assert!(
        second_stdout.contains("no integration target") && second_stdout.contains("Readiness"),
        "ready no-op text should explicitly name the clean no-target state: {second_stdout}"
    );
    assert!(
        !second_stdout.contains("heddle merge main"),
        "ready no-op must not recommend merging the current thread into itself: {second_stdout}"
    );
}

#[test]
fn resolve_without_merge_emits_actionable_json_error() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "resolve", "--all", "--ours"],
        Some(temp.path()),
    )
    .expect("invoke heddle resolve");
    assert!(
        !output.status.success(),
        "resolve with no merge should exit non-zero"
    );
    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    assert!(
        stdout.is_empty(),
        "JSON failure must not pollute stdout: {stdout}"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: serde_json::Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|_| panic!("stderr should be a JSON envelope: {stderr}"));
    assert_eq!(envelope["kind"], "no_merge_in_progress");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("No merge in progress")),
        "error should name the missing merge operation: {envelope}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .unwrap_or("")
            .contains("heddle status"),
        "resolve no-op should point at operation recovery: {envelope}"
    );

    let text = heddle_output(
        &["--output", "text", "resolve", "--all", "--ours"],
        Some(temp.path()),
    )
    .expect("invoke heddle resolve text");
    assert!(
        !text.status.success(),
        "resolve with no merge should exit non-zero in text mode"
    );
    assert!(
        text.stdout.is_empty(),
        "text failure should not write primary output: {}",
        String::from_utf8_lossy(&text.stdout)
    );
    let text_stderr = String::from_utf8_lossy(&text.stderr);
    assert!(
        text_stderr.contains("Error: No merge in progress")
            && text_stderr.contains("Hint:")
            && text_stderr.contains("heddle status")
            && !text_stderr.contains("object not found"),
        "resolve text recovery should name the operation state directly: {text_stderr}"
    );
}

#[test]
fn resolve_with_no_remaining_conflicts_keeps_full_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();
    let repo = Repository::open(temp.path()).unwrap();
    let head = repo.current_state().unwrap().unwrap().change_id;
    let merge_state = repo.merge_state_manager();
    merge_state
        .start(head, head, None, vec!["tracked.txt".to_string()])
        .unwrap();
    merge_state.resolve("tracked.txt").unwrap();

    let output = heddle_output(
        &["--output", "text", "resolve", "--all", "--ours"],
        Some(temp.path()),
    )
    .expect("invoke resolve with no remaining conflicts");
    assert!(
        !output.status.success(),
        "resolve --all with no unresolved conflicts should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "text failure should keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Error: No conflicts to resolve")
            && stderr.contains("Unsafe:")
            && stderr.contains("Would change:")
            && stderr.contains("Preserved:")
            && stderr.contains("Primary recovery: `heddle resolve --list`")
            && stderr.contains("Hint:")
            && stderr.contains("heddle resolve --list"),
        "typed no-conflicts refusal should keep the full advice surface: {stderr}"
    );
}

#[test]
fn heavy_thread_start_explains_non_empty_workspace_recovery() {
    let temp = TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    heddle(&["init"], Some(&repo)).unwrap();
    std::fs::write(repo.join("main.txt"), "main").unwrap();
    heddle(&["capture", "-m", "main"], Some(&repo)).unwrap();

    let target = temp.path().join("already-used");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("draft.txt"), "uncaptured").unwrap();
    let error = heddle(
        &[
            "start",
            "ux-thread",
            "--path",
            target.to_str().expect("path should be utf8"),
        ],
        Some(&repo),
    )
    .expect_err("non-empty materialized worktree should fail with guidance");

    assert!(
        error.contains("is not empty")
            && error.contains("heddle capture")
            && error.contains("heddle start --workspace materialized"),
        "thread start should give premium recovery guidance: {error}"
    );
}

#[test]
fn thread_list_groups_threads_by_user_workflow() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("main.txt"), "main").unwrap();
    heddle(&["capture", "-m", "main"], Some(temp.path())).unwrap();

    let thread_path = temp.path().join("feature-work");
    heddle(
        &[
            "start",
            "feature-work",
            "--path",
            thread_path.to_str().unwrap(),
            "--task",
            "demo",
        ],
        Some(temp.path()),
    )
    .unwrap();
    std::fs::write(thread_path.join("feature.txt"), "feature").unwrap();
    heddle(
        &["capture", "-m", "feature", "--confidence", "0.8"],
        Some(&thread_path),
    )
    .unwrap();

    let output = heddle(&["--output", "text", "thread", "list"], Some(temp.path())).unwrap();
    assert!(
        output.contains("Current"),
        "thread list should group current work: {output}"
    );
    assert!(
        output.contains("Ready to merge"),
        "thread list should group mergeable work: {output}"
    );
    assert!(
        output.contains("next step:"),
        "thread list should use consistent next-step copy: {output}"
    );
    assert!(
        !output.contains("    next:"),
        "thread list should not use the older lowercase next label: {output}"
    );
}

#[test]
fn json_flag_still_renders_json_without_polluting_machine_stderr() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(&["status", "--json"], Some(temp.path())).unwrap();
    assert!(output.status.success(), "status --json should succeed");

    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    let stderr = std::str::from_utf8(&output.stderr).unwrap();

    assert!(
        stdout.trim_start().starts_with('{'),
        "stdout should still be JSON when --json is passed: {stdout}"
    );
    assert!(
        stderr.is_empty(),
        "deprecated --json remains supported but must not pollute machine stderr: {stderr}"
    );
}

#[test]
fn quiet_no_color_and_narrow_text_outputs_preserve_global_contract() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("seed.txt"), "seed\n").unwrap();

    let default_capture = heddle_output(&["capture", "-m", "seed"], Some(temp.path())).unwrap();
    assert!(default_capture.status.success());

    std::fs::write(temp.path().join("more.txt"), "more\n").unwrap();
    let quiet_capture =
        heddle_output(&["--quiet", "capture", "-m", "more"], Some(temp.path())).unwrap();
    assert!(quiet_capture.status.success());
    let quiet_stderr = std::str::from_utf8(&quiet_capture.stderr).unwrap();
    assert!(
        quiet_stderr.is_empty(),
        "--quiet must suppress nonessential tips/logs on stderr: {quiet_stderr}"
    );

    let no_color = heddle_output_with_env(
        &["--output", "text", "status"],
        Some(temp.path()),
        &[("NO_COLOR", "1"), ("CLICOLOR_FORCE", "1")],
    )
    .unwrap();
    assert!(no_color.status.success());
    let stdout = std::str::from_utf8(&no_color.stdout).unwrap();
    let stderr = std::str::from_utf8(&no_color.stderr).unwrap();
    assert!(
        stderr.is_empty(),
        "status text success should keep stderr quiet: {stderr}"
    );
    assert!(
        !stdout.contains('\u{1b}') && !stderr.contains('\u{1b}'),
        "NO_COLOR must override forced color: stdout={stdout:?} stderr={stderr:?}"
    );

    let narrow = heddle_output_with_env(
        &["--output", "text", "status"],
        Some(temp.path()),
        &[("NO_COLOR", "1"), ("COLUMNS", "30")],
    )
    .unwrap();
    assert!(narrow.status.success());
    let narrow_stdout = std::str::from_utf8(&narrow.stdout).unwrap();
    let narrow_stderr = std::str::from_utf8(&narrow.stderr).unwrap();
    assert!(
        narrow_stderr.is_empty(),
        "narrow text status should not need stderr: {narrow_stderr}"
    );
    assert!(
        narrow_stdout.contains("Heddle status") && narrow_stdout.contains("Health:"),
        "narrow status should retain the primary labels: {narrow_stdout}"
    );
    assert!(
        !narrow_stdout.contains('\u{1b}'),
        "NO_COLOR narrow output must not contain ANSI escapes: {narrow_stdout:?}"
    );
}

#[test]
fn narrow_no_color_text_outputs_cover_everyday_read_surfaces() {
    fn assert_text_surface(cwd: &std::path::Path, args: Vec<&str>, needles: &[&str]) {
        let output = heddle_output_with_env(
            &args,
            Some(cwd),
            &[
                ("NO_COLOR", "1"),
                ("CLICOLOR_FORCE", "1"),
                ("COLUMNS", "28"),
            ],
        )
        .unwrap_or_else(|err| panic!("invoke heddle {args:?}: {err}"));
        assert!(
            output.status.success(),
            "narrow text command should succeed for {args:?}; stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "narrow text success should keep stderr quiet for {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            !stdout.contains('\u{1b}'),
            "NO_COLOR must suppress ANSI for {args:?}: {stdout:?}"
        );
        for needle in needles {
            assert!(
                stdout.contains(needle),
                "narrow text output for {args:?} should retain {needle:?}: {stdout}"
            );
        }
    }

    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::create_dir_all(temp.path().join("src/deeply-nested-module")).unwrap();
    std::fs::write(
        temp.path()
            .join("src/deeply-nested-module/very-long-file-name-for-narrow-output.txt"),
        "base\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();
    std::fs::write(
        temp.path()
            .join("src/deeply-nested-module/very-long-file-name-for-narrow-output.txt"),
        "base\nchanged\n",
    )
    .unwrap();

    assert_text_surface(
        temp.path(),
        vec!["--quiet", "--output", "text", "status"],
        &["Heddle status", "Health:"],
    );
    assert_text_surface(
        temp.path(),
        vec!["--quiet", "--output", "text", "diagnose"],
        &["Doctor", "Health:"],
    );
    assert_text_surface(
        temp.path(),
        vec!["--quiet", "--output", "text", "doctor"],
        &["Doctor", "Next step:"],
    );
    assert_text_surface(
        temp.path(),
        vec!["--quiet", "--output", "text", "diff"],
        &["+changed"],
    );
    assert_text_surface(
        temp.path(),
        vec!["--quiet", "--output", "text", "log"],
        &["base"],
    );
    assert_text_surface(
        temp.path(),
        vec!["--quiet", "--output", "text", "show", "HEAD"],
        &["State", "base"],
    );
    assert_text_surface(
        temp.path(),
        vec!["--quiet", "--output", "text", "thread", "list"],
        &["Current"],
    );
    assert_text_surface(
        temp.path(),
        vec!["--quiet", "--output", "text", "workspace", "show"],
        &["Workspace", "main"],
    );
    assert_text_surface(
        temp.path(),
        vec!["--quiet", "--output", "text", "bridge", "git", "status"],
        &["Repository mode", "Git import"],
    );
    assert_text_surface(
        temp.path(),
        vec!["--quiet", "--output", "text", "fsck", "--bridge"],
        &["repository is valid", "Bridge:"],
    );

    let ready = heddle_output_with_env(
        &["--quiet", "--output", "text", "ready"],
        Some(temp.path()),
        &[
            ("NO_COLOR", "1"),
            ("CLICOLOR_FORCE", "1"),
            ("COLUMNS", "28"),
        ],
    )
    .expect("invoke ready narrow text");
    assert!(ready.status.success(), "ready should succeed");
    assert!(ready.stderr.is_empty(), "ready should keep stderr quiet");
    let ready_stdout = String::from_utf8_lossy(&ready.stdout);
    assert!(
        !ready_stdout.contains('\u{1b}') && ready_stdout.contains("Readiness"),
        "ready narrow text should be no-color and retain labels: {ready_stdout}"
    );
    assert!(
        !ready_stdout.contains("heddle merge main"),
        "ready narrow text must avoid stale self-merge guidance: {ready_stdout}"
    );
}

#[test]
fn default_run_does_not_leak_info_traces() {
    let temp = TempDir::new().unwrap();
    let output = heddle_output(&["init"], Some(temp.path())).unwrap();
    assert!(output.status.success(), "init should succeed");

    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    assert!(
        !stderr.contains("INFO"),
        "default verbosity should suppress INFO traces (got: {stderr:?})"
    );
}

#[test]
fn verbose_flag_re_enables_info_traces() {
    let temp = TempDir::new().unwrap();
    let output = heddle_output(&["-v", "init"], Some(temp.path())).unwrap();
    assert!(output.status.success(), "init -v should succeed");

    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    assert!(
        stderr.contains("INFO"),
        "-v should restore INFO-level traces (got: {stderr:?})"
    );
}

#[test]
fn missing_repo_status_emits_hint_in_text_mode() {
    let temp = TempDir::new().unwrap();
    let output =
        heddle_output(&["--output", "text", "status"], Some(temp.path())).expect("invoke heddle");
    assert!(
        !output.status.success(),
        "status on non-repo dir should exit non-zero"
    );

    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    assert!(
        stderr.contains("Error:"),
        "stderr should carry an Error: line: {stderr}"
    );
    assert!(
        stderr.contains("repository not found"),
        "stderr should name the actual failure: {stderr}"
    );
    assert!(
        stderr.contains("Hint:") && stderr.contains("heddle init"),
        "stderr should suggest `heddle init`: {stderr}"
    );
}

#[test]
fn missing_repo_status_emits_structured_error_in_json_mode() {
    let temp = TempDir::new().unwrap();
    let output =
        heddle_output(&["--output", "json", "status"], Some(temp.path())).expect("invoke heddle");
    assert!(
        !output.status.success(),
        "status on non-repo dir should exit non-zero"
    );

    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: serde_json::Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|_| panic!("stderr should be a single-line JSON envelope: {stderr}"));
    assert_eq!(envelope["kind"], "repository_not_found");
    assert!(
        envelope["error"]
            .as_str()
            .unwrap_or("")
            .contains("repository not found"),
        "envelope.error should name the failure: {envelope}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .unwrap_or("")
            .contains("heddle init"),
        "envelope.hint should suggest heddle init: {envelope}"
    );
}

#[test]
fn missing_repo_path_emits_actionable_json_error_envelope() {
    let temp = TempDir::new().unwrap();
    let missing = temp.path().join("missing-repo");
    let output = heddle_output(
        &[
            "--repo",
            missing.to_str().expect("path should be utf8"),
            "--output",
            "json",
            "status",
        ],
        None,
    )
    .expect("invoke heddle");
    assert!(
        !output.status.success(),
        "status on a missing --repo path should exit non-zero"
    );

    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    assert!(
        stdout.is_empty(),
        "JSON failure must not pollute stdout: {stdout}"
    );

    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: serde_json::Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|_| panic!("stderr should be a single-line JSON envelope: {stderr}"));
    assert_eq!(envelope["kind"], "path_not_found");
    assert!(
        envelope["hint"].as_str().unwrap_or("").contains("--repo"),
        "missing path errors should point at --repo recovery: {envelope}"
    );
}

#[test]
fn global_flags_only_renders_curated_help_not_clap_error() {
    // The user typed `heddle --output text` with no subcommand. Without the
    // intercept, clap would dump a 60+ verb wall of text. With it, the
    // curated everyday-verb help renders cleanly.
    let output = heddle_output(&["--output", "text"], None).expect("invoke heddle");
    assert!(
        output.status.success(),
        "global-flags-only invocation should print help and exit 0"
    );

    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    assert!(
        stdout.contains("Heddle") && stdout.contains("Everyday commands:"),
        "curated help should render: stdout={stdout}"
    );
    for verb in ["workspace", "ready", "diff", "resolve", "doctor"] {
        assert!(
            stdout.contains(&format!("\n  {verb}")),
            "core-loop verb `{verb}` should be on the curated surface: {stdout}"
        );
    }
    for verb in ["review", "discuss", "context", "goto"] {
        assert!(
            !stdout.contains(&format!("\n  {verb}")),
            "non-core verb `{verb}` should stay behind advanced/topic help: {stdout}"
        );
    }
    assert!(
        !stdout.contains("error: 'heddle' requires a subcommand"),
        "clap's missing-subcommand error must not surface: stdout={stdout}"
    );
    assert!(
        !stderr.contains("error: 'heddle' requires a subcommand"),
        "clap's missing-subcommand error must not surface on stderr: stderr={stderr}"
    );
}

#[test]
fn workspace_bare_command_defaults_to_control_tower() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let text = heddle(&["--output", "text", "workspace"], Some(temp.path()))
        .expect("bare workspace should render the control tower");
    assert!(
        text.contains("Workspace:")
            && text.contains("Current thread")
            && text.contains("Threads in flight:"),
        "bare workspace should behave like workspace show, not print subcommand help: {text}"
    );

    let json = heddle(&["--output", "json", "workspace"], Some(temp.path()))
        .expect("bare workspace should support JSON through the default show view");
    let parsed: Value = serde_json::from_str(&json)
        .unwrap_or_else(|_| panic!("bare workspace JSON should parse: {json}"));
    assert!(
        parsed["repository_capability"].as_str().is_some(),
        "workspace JSON should identify repository capability: {json}"
    );
    assert!(
        parsed["groups"].is_array(),
        "workspace JSON should expose groups: {json}"
    );
}

#[test]
fn command_catalog_exposes_public_surface_for_agents() {
    let json = heddle(&["commands", "--output", "json"], None)
        .expect("command catalog JSON should succeed");
    let parsed: Value = serde_json::from_str(&json)
        .unwrap_or_else(|_| panic!("command catalog JSON should parse: {json}"));
    let commands = parsed["commands"]
        .as_array()
        .expect("commands should be an array");
    assert!(
        commands.len() > 40,
        "catalog should enumerate the public command tree: {json}"
    );
    let status = commands
        .iter()
        .find(|entry| entry["display"] == "status")
        .expect("status command should be cataloged");
    assert_eq!(status["tier"], "everyday");
    assert!(
        status["options"]
            .as_array()
            .unwrap()
            .iter()
            .any(|option| option["long"] == "short" && option["short"] == "s"),
        "status options should include --short/-s: {status}"
    );
    assert!(
        parsed["global_options"]
            .as_array()
            .unwrap()
            .iter()
            .any(|option| option["long"] == "output"),
        "catalog should expose global --output: {json}"
    );

    let text = heddle(&["commands", "--output", "text"], None)
        .expect("command catalog text should succeed");
    assert!(
        text.contains("Command catalog")
            && text.contains("everyday:")
            && text.contains("advanced:")
            && text.contains("commands"),
        "command catalog text should be scannable: {text}"
    );
}

#[test]
fn git_dependencies_help_topic_explains_no_git_contract() {
    let help = heddle(&["help", "git-dependencies"], None)
        .expect("git-dependencies help topic should render");
    assert!(
        help.contains("without `git` on PATH")
            && help.contains("optional escape hatches")
            && help.contains("merge --git-commit")
            && help.contains("heddle commands --output json"),
        "git-dependencies topic should explain supported paths and escape hatches: {help}"
    );
}

#[test]
fn unknown_flag_alone_still_routes_to_clap_error() {
    // The intercept must NOT swallow real parse errors — typing
    // `heddle --invalid-flag` should still surface the clap error so the
    // typo is obvious.
    let output = heddle_output(&["--invalid-flag"], None).expect("invoke heddle");
    assert!(
        !output.status.success(),
        "unknown flag should exit non-zero"
    );

    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    assert!(
        stderr.contains("unexpected argument") || stderr.contains("--invalid-flag"),
        "clap should name the offending flag: stderr={stderr}"
    );
}

#[test]
fn start_emits_cd_hint_in_text_mode() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("seed.txt"), "seed").unwrap();
    heddle(
        &["capture", "-m", "seed", "--confidence", "0.9"],
        Some(temp.path()),
    )
    .unwrap();

    let output = heddle(
        &["--output", "text", "start", "scratch-thread"],
        Some(temp.path()),
    )
    .expect("start scratch-thread");
    assert!(
        output.contains("Path:"),
        "text-mode start should print the checkout path: {output}"
    );
    assert!(
        output.contains("Run this to switch shells:"),
        "text-mode start should suggest the cd command: {output}"
    );
    assert!(
        output.contains("    cd "),
        "the cd hint should include the literal `cd` invocation: {output}"
    );
}

#[test]
fn cd_hint_quotes_paths_with_spaces() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("seed.txt"), "seed").unwrap();
    heddle(
        &["capture", "-m", "seed", "--confidence", "0.9"],
        Some(temp.path()),
    )
    .unwrap();

    let checkout = temp.path().join("scratch dir");
    let checkout_str = checkout.to_str().expect("utf-8 path");
    let output = heddle(
        &[
            "--output",
            "text",
            "start",
            "spaced-thread",
            "--path",
            checkout_str,
        ],
        Some(temp.path()),
    )
    .expect("start with spaced path");

    let quoted = format!("'{checkout_str}'");
    assert!(
        output.contains(&format!("    cd {quoted}")),
        "cd hint must single-quote paths with spaces: {output}"
    );
}

#[test]
fn start_print_cd_path_returns_only_the_path() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("seed.txt"), "seed").unwrap();
    heddle(
        &["capture", "-m", "seed", "--confidence", "0.9"],
        Some(temp.path()),
    )
    .unwrap();

    let output = heddle_output(
        &["start", "scratch-cd", "--print-cd-path"],
        Some(temp.path()),
    )
    .expect("start --print-cd-path");
    assert!(
        output.status.success(),
        "start --print-cd-path should succeed"
    );

    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    let trimmed = stdout.trim();
    assert!(
        trimmed.contains("scratch-cd"),
        "stdout should be a path referencing the new thread name: {stdout:?}"
    );
    // Pure-path output: no embedded JSON, no labels, no extra prose.
    assert!(
        !trimmed.contains('{'),
        "stdout must not contain JSON when --print-cd-path is set: {stdout:?}"
    );
    assert!(
        !trimmed.contains("Path:"),
        "stdout must not contain the human label when --print-cd-path is set: {stdout:?}"
    );
    assert_eq!(
        trimmed.lines().count(),
        1,
        "stdout should be a single line: {stdout:?}"
    );
}

#[test]
fn unknown_state_id_hints_at_heddle_log_across_state_readers() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for args in [
        vec!["--output", "text", "goto", "hd-nonexistent"],
        vec!["--output", "text", "show", "hd-nonexistent"],
        vec!["--output", "text", "diff", "hd-nonexistent", "HEAD"],
    ] {
        let output = heddle_output(&args, Some(temp.path()))
            .unwrap_or_else(|err| panic!("invoke heddle {args:?}: {err}"));
        assert!(
            !output.status.success(),
            "missing state should exit non-zero for {args:?}"
        );
        assert!(
            output.stdout.is_empty(),
            "missing-state failures should not write primary output for {args:?}: {}",
            String::from_utf8_lossy(&output.stdout)
        );

        let stderr = std::str::from_utf8(&output.stderr).unwrap();
        assert!(
            stderr.contains("State not found"),
            "stderr should carry the original error for {args:?}: {stderr}"
        );
        assert!(
            stderr.contains("Hint:") && stderr.contains("heddle log"),
            "stderr should suggest `heddle log` for {args:?}: {stderr}"
        );
    }
}

#[test]
fn unknown_thread_hints_at_heddle_thread_list() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "text", "thread", "show", "missing"],
        Some(temp.path()),
    )
    .expect("invoke heddle thread show");
    assert!(
        !output.status.success(),
        "thread show on a missing thread should exit non-zero"
    );

    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    assert!(
        stderr.contains("Thread 'missing' not found"),
        "stderr should carry the original error: {stderr}"
    );
    assert!(
        stderr.contains("Hint:") && stderr.contains("heddle thread list"),
        "stderr should suggest `heddle thread list`: {stderr}"
    );

    let json = heddle_output(
        &["--output", "json", "thread", "show", "missing"],
        Some(temp.path()),
    )
    .expect("invoke heddle thread show json");
    assert!(
        !json.status.success(),
        "thread show on a missing thread should exit non-zero"
    );
    assert!(
        json.stdout.is_empty(),
        "JSON-mode missing thread show refusal must keep stdout quiet"
    );
    let stderr = std::str::from_utf8(&json.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("missing thread show should emit JSON envelope");
    assert_eq!(envelope["kind"], "thread_not_found");
    assert!(
        envelope["error"].as_str().is_some_and(|error| error
            .contains("Thread 'missing' not found")
            && error.contains("Unsafe:")
            && error.contains("Would change:")
            && error.contains("Preserved:")
            && error.contains("Primary recovery:")),
        "missing thread show should include typed recovery detail: {stderr}"
    );
}

#[test]
fn help_for_verb_prefixes_usage_with_heddle() {
    // `heddle help status` falls through to status's clap-derived help.
    // The Usage line MUST start with `Usage: heddle status` — saying just
    // `Usage: status` would suggest the user can run `status` standalone.
    for verb in ["status", "capture", "log", "merge", "undo", "start", "init"] {
        let output =
            heddle(&["help", verb], None).unwrap_or_else(|err| panic!("heddle help {verb}: {err}"));
        assert!(
            output.contains(&format!("Usage: heddle {verb}")),
            "`heddle help {verb}` must prefix the Usage line with `heddle`: {output}"
        );
    }
}

#[test]
fn public_command_paths_have_all_required_help_entrypoints() {
    let paths = public_command_paths();
    assert!(
        paths.len() > 40,
        "public help coverage should enumerate the real command tree, got {paths:?}"
    );

    for path in paths {
        let display = path.join(" ");

        let mut help_args: Vec<&str> = Vec::with_capacity(path.len() + 1);
        help_args.push("help");
        help_args.extend(path.iter().map(String::as_str));
        let output = heddle_output(&help_args, None)
            .unwrap_or_else(|err| panic!("heddle help {display} should run: {err}"));
        assert!(
            output.status.success(),
            "heddle help {display} should exit 0: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "heddle help {display} must write help to stdout only: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            !stdout.trim().is_empty() && !stdout.contains("no topic or command"),
            "heddle help {display} should render useful command help: {stdout}"
        );

        for flag in ["--help", "-h"] {
            let mut args: Vec<&str> = path.iter().map(String::as_str).collect();
            args.push(flag);
            let output = heddle_output(&args, None)
                .unwrap_or_else(|err| panic!("heddle {display} {flag} should run: {err}"));
            assert!(
                output.status.success(),
                "heddle {display} {flag} should exit 0: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                output.stderr.is_empty(),
                "heddle {display} {flag} must write help to stdout only: stderr={}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                stdout.contains("Usage:") && stdout.contains("heddle"),
                "heddle {display} {flag} should render command usage: {stdout}"
            );
        }
    }
}

#[test]
fn public_command_paths_have_command_contract_metadata() {
    let catalog = cli::cli::commands::build_command_catalog();
    let catalog_paths = catalog
        .commands
        .iter()
        .map(|entry| entry.path.clone())
        .collect::<std::collections::BTreeSet<_>>();

    for path in public_command_paths() {
        assert!(
            catalog_paths.contains(&path),
            "public command `{}` must have command contract metadata",
            path.join(" ")
        );
    }
}

fn public_command_paths() -> Vec<Vec<String>> {
    fn walk(command: &clap::Command, prefix: &mut Vec<String>, paths: &mut Vec<Vec<String>>) {
        for subcommand in command.get_subcommands().filter(|cmd| !cmd.is_hide_set()) {
            prefix.push(subcommand.get_name().to_string());
            paths.push(prefix.clone());
            walk(subcommand, prefix, paths);
            prefix.pop();
        }
    }

    let command = Cli::command();
    let mut paths = Vec::new();
    walk(&command, &mut Vec::new(), &mut paths);
    paths
}

#[test]
fn everyday_commands_have_all_required_help_entrypoints() {
    let everyday = [
        "init",
        "status",
        "start",
        "capture",
        "checkpoint",
        "log",
        "show",
        "diff",
        "merge",
        "resolve",
        "undo",
        "thread",
        "bridge",
        "doctor",
        "diagnose",
        "help",
        "version",
    ];

    for verb in everyday {
        let topic = heddle(&["help", verb], None)
            .unwrap_or_else(|err| panic!("heddle help {verb} should succeed: {err}"));
        assert!(
            !topic.trim().is_empty() && !topic.contains("no topic"),
            "heddle help {verb} should render useful help: {topic}"
        );

        for flag in ["--help", "-h"] {
            let output = heddle(&[verb, flag], None)
                .unwrap_or_else(|err| panic!("heddle {verb} {flag} should succeed: {err}"));
            assert!(
                output.contains("Usage:") && output.contains("heddle") && output.contains(verb),
                "heddle {verb} {flag} should render command help with usage: {output}"
            );
        }
    }
}

#[test]
fn context_get_honors_user_config_principal_not_unknown() {
    // Regression: `heddle context set` / `context get` used to route through
    // `repo.get_attribution()`, which only consults env + repo config.
    // A user with `[principal]` only in `~/.config/heddle/config.toml` saw
    // every annotation surface as `Unknown <unknown@example.com>`. After
    // the migration to `resolve_attribution`, the user-config principal
    // wins as it does for `heddle capture`.
    let temp = TempDir::new().unwrap();
    let user_cfg_dir = temp.path().join(".heddle-user");
    std::fs::create_dir_all(&user_cfg_dir).unwrap();
    std::fs::write(
        user_cfg_dir.join("config.toml"),
        "[principal]\nname = \"Ada\"\nemail = \"ada@example.com\"\n",
    )
    .unwrap();

    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("main.rs"), "fn main() {}").unwrap();
    heddle(
        &["capture", "-m", "seed", "--confidence", "0.9"],
        Some(temp.path()),
    )
    .unwrap();
    heddle(
        &[
            "context",
            "set",
            "--path",
            "main.rs",
            "--scope",
            "file",
            "--kind",
            "rationale",
            "-m",
            "entry point",
        ],
        Some(temp.path()),
    )
    .unwrap();

    let output = heddle(
        &["--output", "text", "context", "get", "--path", "main.rs"],
        Some(temp.path()),
    )
    .expect("context get");
    assert!(
        output.contains("by: Ada <ada@example.com>"),
        "context get should attribute the annotation to the user-config principal: {output}"
    );
    assert!(
        !output.contains("Unknown <unknown@example.com>"),
        "context get must not fall back to Unknown when user config has a principal: {output}"
    );
}

#[test]
fn context_invalid_scope_uses_typed_advice_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("main.rs"), "fn main() {}\n").unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &[
            "--output",
            "json",
            "context",
            "set",
            "--path",
            "main.rs",
            "--scope",
            "symbol:",
            "-m",
            "empty symbol",
        ],
        Some(temp.path()),
    )
    .expect("invoke invalid context scope");
    assert!(
        !output.status.success(),
        "invalid context scope should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode context scope refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("context scope refusal should emit JSON envelope");
    assert_eq!(envelope["kind"], "context_symbol_name_required");
    assert!(
        envelope["error"].as_str().is_some_and(|error| error
            .contains("Symbol name must not be empty")
            && error.contains("Unsafe:")
            && error.contains("Would change:")
            && error.contains("Preserved:")
            && error.contains("Primary recovery:")),
        "context scope refusal should include typed recovery detail: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("symbol:<name>")),
        "context scope hint should explain the valid symbol form: {stderr}"
    );
}

#[test]
fn integration_invalid_harness_uses_typed_advice_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init");
    let output = heddle_output(
        &[
            "--output",
            "json",
            "integration",
            "install",
            "unknown-harness",
        ],
        Some(temp.path()),
    )
    .expect("invoke integration install");
    assert!(!output.status.success(), "unsupported harness must refuse");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode refusal must not write stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: Value =
        serde_json::from_str(&stderr).expect("stderr should be JSON error envelope");
    assert_eq!(envelope["kind"], "integration_harness_unsupported");
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("codex")
                && hint.contains("claude-code")
                && hint.contains("opencode")),
        "typed advice should name supported harnesses: {stderr}"
    );
}

#[test]
fn integration_codex_repo_scope_uses_typed_advice_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init");
    let output = heddle_output(
        &[
            "--output",
            "json",
            "integration",
            "install",
            "codex",
            "--scope",
            "repo",
        ],
        Some(temp.path()),
    )
    .expect("invoke integration install");
    assert!(
        !output.status.success(),
        "codex repo-scope install must refuse"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode refusal must not write stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: Value =
        serde_json::from_str(&stderr).expect("stderr should be JSON error envelope");
    assert_eq!(envelope["kind"], "integration_codex_scope_invalid");
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("--scope user")),
        "typed advice should name user-scope recovery: {stderr}"
    );
}

#[test]
fn agent_serve_background_uses_typed_advice_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init");
    let output = heddle_output(&["--output", "json", "agent", "serve"], Some(temp.path()))
        .expect("invoke agent serve");
    assert!(
        !output.status.success(),
        "background agent serve must refuse"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode refusal must not write stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: Value =
        serde_json::from_str(&stderr).expect("stderr should be JSON error envelope");
    assert_eq!(envelope["kind"], "agent_background_unimplemented");
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("agent serve --foreground")),
        "typed advice should name foreground recovery: {stderr}"
    );
}

#[test]
fn agent_stop_invalid_pidfile_uses_typed_advice_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init");
    let sockets = temp.path().join(".heddle/sockets");
    std::fs::create_dir_all(&sockets).expect("create sockets dir");
    std::fs::write(sockets.join("grpc.pid"), "not-a-heddle-pidfile\n").expect("write pidfile");

    let output = heddle_output(&["--output", "json", "agent", "stop"], Some(temp.path()))
        .expect("invoke agent stop");
    assert!(!output.status.success(), "invalid pidfile must refuse");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode refusal must not write stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: Value =
        serde_json::from_str(&stderr).expect("stderr should be JSON error envelope");
    assert_eq!(envelope["kind"], "agent_pidfile_invalid");
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("pidfile")),
        "typed advice should explain pidfile recovery: {stderr}"
    );
}

#[test]
fn agent_heartbeat_missing_session_uses_typed_advice_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init");
    let output = heddle_output(
        &[
            "--output",
            "json",
            "agent",
            "heartbeat",
            "--session",
            "missing-session",
        ],
        Some(temp.path()),
    )
    .expect("invoke agent heartbeat");
    assert!(!output.status.success(), "missing session must refuse");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode refusal must not write stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: Value =
        serde_json::from_str(&stderr).expect("stderr should be JSON error envelope");
    assert_eq!(envelope["kind"], "agent_session_not_found");
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("Reserve the thread again")),
        "typed advice should name reservation recovery: {stderr}"
    );
}

#[test]
fn default_auto_output_is_json_when_stdout_is_piped_and_text_when_forced() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("work.txt"), "pending").unwrap();

    let auto = heddle_output(&["status"], Some(temp.path())).expect("invoke auto status");
    assert!(auto.status.success(), "auto status should succeed");
    assert!(
        auto.stderr.is_empty(),
        "auto JSON stdout must not be accompanied by stderr prose: {}",
        String::from_utf8_lossy(&auto.stderr)
    );
    let auto_stdout = String::from_utf8_lossy(&auto.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&auto_stdout)
        .unwrap_or_else(|_| panic!("piped auto status should emit JSON: {auto_stdout}"));
    assert_eq!(parsed["thread_health"], "uncaptured");
    assert_eq!(parsed["changed_path_count"], 1);
    assert_eq!(parsed["changes"]["added"].as_array().map(Vec::len), Some(1));

    let text = heddle_output(&["--output", "text", "status"], Some(temp.path()))
        .expect("invoke forced-text status");
    assert!(text.status.success(), "forced text status should succeed");
    let text_stdout = String::from_utf8_lossy(&text.stdout);
    assert!(
        text_stdout.contains("Heddle status"),
        "--output text should override piped auto JSON: {text_stdout}"
    );
}

#[test]
fn tty_auto_mode_renders_text_and_explicit_json_stays_json() {
    let script_probe = std::process::Command::new("script")
        .arg("--version")
        .output();
    let Ok(probe) = script_probe else {
        eprintln!("skipping tty transcript test: util-linux script not installed");
        return;
    };
    let probe_stdout = String::from_utf8_lossy(&probe.stdout);
    if !probe.status.success() || !probe_stdout.contains("util-linux") {
        eprintln!("skipping tty transcript test: unsupported script implementation");
        return;
    }

    let temp = TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    heddle(&["init"], Some(&repo)).unwrap();
    std::fs::write(repo.join("app.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(&repo)).unwrap();

    let binary = env!("CARGO_BIN_EXE_heddle");
    let config = repo.join(".heddle-user/config.toml");
    let repo_arg = repo.to_str().expect("repo path should be utf8");
    let config_arg = config.to_str().expect("config path should be utf8");

    let text_cmd = format!(
        "NO_COLOR=1 COLUMNS=40 HEDDLE_CONFIG={config_arg} {binary} --repo {repo_arg} status"
    );
    let text = std::process::Command::new("script")
        .args(["-q", "-e", "-c", &text_cmd, "/dev/null"])
        .output()
        .expect("run status under script tty");
    assert!(
        text.status.success(),
        "tty status should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&text.stdout),
        String::from_utf8_lossy(&text.stderr)
    );
    let text_stdout = String::from_utf8_lossy(&text.stdout);
    assert!(
        text_stdout.contains("Heddle status")
            && text_stdout.contains("Health:")
            && !text_stdout.trim_start().starts_with('{')
            && !text_stdout.contains('\u{1b}'),
        "auto mode on a TTY should render no-color human text: {text_stdout:?}"
    );

    let json_cmd = format!(
        "NO_COLOR=1 COLUMNS=40 HEDDLE_CONFIG={config_arg} {binary} --repo {repo_arg} --output json status"
    );
    let json = std::process::Command::new("script")
        .args(["-q", "-e", "-c", &json_cmd, "/dev/null"])
        .output()
        .expect("run explicit-json status under script tty");
    assert!(
        json.status.success(),
        "tty explicit JSON status should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&json.stdout),
        String::from_utf8_lossy(&json.stderr)
    );
    let json_stdout = String::from_utf8_lossy(&json.stdout);
    let parsed: serde_json::Value = serde_json::from_str(json_stdout.trim())
        .unwrap_or_else(|_| panic!("explicit JSON under TTY should parse: {json_stdout:?}"));
    assert_eq!(parsed["thread_health"], "clean");

    let checkout = temp.path().join("tty-thread");
    let checkout_arg = checkout.to_str().expect("checkout path should be utf8");
    let start_cmd = format!(
        "NO_COLOR=1 COLUMNS=40 HEDDLE_CONFIG={config_arg} {binary} --repo {repo_arg} start tty-thread --workspace solid --path {checkout_arg}"
    );
    let start = std::process::Command::new("script")
        .args(["-q", "-e", "-c", &start_cmd, "/dev/null"])
        .output()
        .expect("run start under script tty");
    assert!(
        start.status.success(),
        "tty start should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr)
    );
    let start_stdout = String::from_utf8_lossy(&start.stdout);
    assert!(
        start_stdout.contains("Started heavy thread 'tty-thread'")
            && start_stdout.contains("Path:")
            && start_stdout.contains("Next step:")
            && start_stdout.contains("heddle ready --thread tty-thread")
            && !start_stdout.contains('\u{1b}'),
        "start on a TTY should render no-color human guidance: {start_stdout:?}"
    );
}

#[test]
fn global_exit_codes_and_failure_streams_are_predictable() {
    let help = heddle_output(&["help", "status"], None).expect("invoke help");
    assert_eq!(help.status.code(), Some(0));
    assert!(
        help.stderr.is_empty(),
        "help should write to stdout only: {}",
        String::from_utf8_lossy(&help.stderr)
    );
    assert!(String::from_utf8_lossy(&help.stdout).contains("Usage: heddle status"));

    let typo = heddle_output(&["statuz"], None).expect("invoke typo");
    assert_eq!(typo.status.code(), Some(2));
    assert!(
        typo.stdout.is_empty(),
        "parse errors should not write primary output: {}",
        String::from_utf8_lossy(&typo.stdout)
    );
    let typo_stderr = String::from_utf8_lossy(&typo.stderr);
    assert!(
        typo_stderr.contains("unrecognized subcommand") && typo_stderr.contains("status"),
        "parse errors should name the problem and suggest likely commands: {typo_stderr}"
    );

    let temp = TempDir::new().unwrap();
    let missing_repo = heddle_output(&["--output", "json", "status"], Some(temp.path()))
        .expect("invoke missing-repo status");
    assert_eq!(missing_repo.status.code(), Some(1));
    assert!(
        missing_repo.stdout.is_empty(),
        "JSON-mode failures must keep stdout clean: {}",
        String::from_utf8_lossy(&missing_repo.stdout)
    );
    let stderr = String::from_utf8_lossy(&missing_repo.stderr);
    let envelope: serde_json::Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|_| panic!("stderr should be a JSON envelope: {stderr}"));
    assert_eq!(envelope["kind"], "repository_not_found");
    assert!(
        envelope["hint"]
            .as_str()
            .unwrap_or("")
            .contains("heddle init"),
        "environment failures should include a recovery hint: {envelope}"
    );
}

#[test]
fn fsck_on_corrupt_ref_emits_integrity_hint_in_text_and_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(
        temp.path().join(".heddle/refs/threads/main"),
        "bad-state-id",
    )
    .unwrap();

    let json = heddle_output(&["--output", "json", "fsck"], Some(temp.path()))
        .expect("invoke corrupt fsck json");
    assert!(
        !json.status.success(),
        "corrupt fsck JSON should exit non-zero"
    );
    assert!(
        json.stdout.is_empty(),
        "JSON failure must not pollute stdout: {}",
        String::from_utf8_lossy(&json.stdout)
    );
    let json_stderr = String::from_utf8_lossy(&json.stderr);
    let envelope: serde_json::Value = serde_json::from_str(json_stderr.trim())
        .unwrap_or_else(|_| panic!("stderr should be JSON envelope: {json_stderr}"));
    assert_eq!(envelope["kind"], "repository_integrity_error");
    assert!(
        envelope["error"]
            .as_str()
            .unwrap_or("")
            .contains("invalid object"),
        "corrupt ref should preserve the original failure: {envelope}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .unwrap_or("")
            .contains("heddle fsck --full"),
        "corrupt ref should point at fsck recovery: {envelope}"
    );

    let text = heddle_output(&["--output", "text", "fsck"], Some(temp.path()))
        .expect("invoke corrupt fsck text");
    assert!(
        !text.status.success(),
        "corrupt fsck text should exit non-zero"
    );
    assert!(
        text.stdout.is_empty(),
        "text failure must not write primary output: {}",
        String::from_utf8_lossy(&text.stdout)
    );
    let text_stderr = String::from_utf8_lossy(&text.stderr);
    assert!(
        text_stderr.contains("Error: invalid object")
            && text_stderr.contains("Hint:")
            && text_stderr.contains("heddle fsck --full"),
        "corrupt ref text recovery should include original error and fsck hint: {text_stderr}"
    );
}

#[test]
fn error_envelope_schema_is_registered_and_matches_runtime_shape() {
    // The error envelope is the stderr contract for JSON-mode failures.
    // `heddle schemas error` returns its mirror schema; the fields it
    // declares MUST match what `print_error_with_hint` actually emits.
    let schema = heddle(&["schemas", "error"], None).expect("heddle schemas error");
    let parsed: serde_json::Value = serde_json::from_str(&schema).expect("schema parses");
    let props = parsed["properties"]
        .as_object()
        .expect("schema has properties");
    for field in ["error", "hint", "kind"] {
        assert!(
            props.contains_key(field),
            "ErrorEnvelopeSchema must declare `{field}`: {schema}"
        );
    }
    let required: Vec<&str> = parsed["required"]
        .as_array()
        .expect("schema lists required fields")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        required.contains(&"error"),
        "`error` must be required: {schema}"
    );
    assert!(
        required.contains(&"hint"),
        "`hint` must be required: {schema}"
    );
    assert!(
        required.contains(&"kind"),
        "`kind` must be required: {schema}"
    );

    // And the runtime really emits this shape: trigger a known failure
    // class and parse the stderr envelope.
    let temp = TempDir::new().unwrap();
    let output = heddle_output(&["--output", "json", "status"], Some(temp.path()))
        .expect("invoke heddle status");
    assert!(!output.status.success());
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: serde_json::Value =
        serde_json::from_str(stderr.trim()).expect("stderr is a JSON object");
    for field in ["error", "hint", "kind"] {
        assert!(
            envelope.get(field).is_some(),
            "envelope must carry `{field}` field per the schema: {stderr}"
        );
    }
    assert_eq!(envelope["kind"], "repository_not_found");
}

#[test]
fn generic_json_runtime_errors_keep_nonempty_machine_envelope() {
    let output = heddle_output(&["--output", "json", "schemas", "not-a-schema"], None)
        .expect("invoke missing schema");
    assert!(!output.status.success(), "missing schema should fail");
    assert!(
        output.stdout.is_empty(),
        "JSON failure must not pollute stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: serde_json::Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|err| panic!("stderr should be JSON: {err}: {stderr}"));
    assert_eq!(envelope["kind"], "runtime_error");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("no schema registered")),
        "runtime error envelope should preserve the original error: {envelope}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| !hint.trim().is_empty()),
        "runtime error envelope must carry a non-empty hint: {envelope}"
    );
}

#[test]
fn doctor_schemas_has_no_drift_or_unmatched_registered_verbs() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root");
    let repo_arg = repo_root.to_str().expect("workspace root should be utf8");
    let output = heddle(
        &["--repo", repo_arg, "doctor", "schemas", "--output", "json"],
        Some(repo_root),
    )
    .expect("heddle doctor schemas --output json");
    let parsed: serde_json::Value = serde_json::from_str(&output)
        .unwrap_or_else(|_| panic!("doctor schemas should emit JSON: {output}"));

    assert_eq!(
        parsed["issues"].as_array().map(Vec::len),
        Some(0),
        "schema docs must not have drift findings: {output}"
    );
    assert_eq!(
        parsed["unmatched_verbs"].as_array().map(Vec::len),
        Some(0),
        "every registered schema verb must have a parseable documented sample: {output}"
    );
}

#[test]
fn status_text_hides_capture_durability_local_only_by_default() {
    // The fallback "Capture durability: local only" line repeated on
    // every `heddle status` against a non-checkpointed state — pure
    // noise since the absence of a `Git checkpoint:` line already
    // encodes the same information. Hidden by default; `-v` brings it
    // back. JSON output is unchanged (the field is on the wire shape).
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("a"), "1").unwrap();
    heddle(
        &["capture", "-m", "seed", "--confidence", "0.9"],
        Some(temp.path()),
    )
    .unwrap();

    let default =
        heddle(&["--output", "text", "status"], Some(temp.path())).expect("status default");
    assert!(
        !default.contains("Capture durability:"),
        "default status must not show the local-only fallback: {default}"
    );

    let verbose =
        heddle(&["--output", "text", "-v", "status"], Some(temp.path())).expect("status -v");
    assert!(
        verbose.contains("Capture durability: local only"),
        "-v status must surface the durability line: {verbose}"
    );
}

#[test]
fn blame_drops_email_when_attribution_overflows_column() {
    // `Luke Thorne <the.thorne48@gmail.com>` blew the 20-char column,
    // truncating to `Luke Thorne <the...` — keeping the noise and
    // dropping the signal. The fit_author helper drops the email
    // entirely when the name alone fits the column.
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(
        temp.path().join(".heddle-user/config.toml"),
        "[principal]\nname = \"Ada Lovelace\"\nemail = \"ada@really.long.example.com\"\n",
    )
    .unwrap_or(()); // best-effort; harness already wrote a config we'll override
    let cfg_dir = temp.path().join(".heddle-user");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("config.toml"),
        "[principal]\nname = \"Ada Lovelace\"\nemail = \"ada@really.long.example.com\"\n",
    )
    .unwrap();
    std::fs::write(temp.path().join("note.txt"), "first line\nsecond line\n").unwrap();
    heddle(
        &["capture", "-m", "seed", "--confidence", "0.9"],
        Some(temp.path()),
    )
    .unwrap();

    let output = heddle(
        &["--output", "text", "blame", "note.txt"],
        Some(temp.path()),
    )
    .expect("blame note.txt");
    assert!(
        output.contains("Ada Lovelace"),
        "blame must show the principal name: {output}"
    );
    assert!(
        !output.contains("Ada Loveli...") && !output.contains("Ada Lovela..."),
        "blame must not mid-name-truncate when the name itself fits: {output}"
    );
    assert!(
        !output.contains("really.long"),
        "blame must drop the email when the name fits the column: {output}"
    );
}

#[test]
fn blame_missing_file_uses_typed_advice_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    heddle(&["capture", "-m", "tracked"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "blame", "missing.txt"],
        Some(temp.path()),
    )
    .expect("invoke missing blame");
    assert!(!output.status.success(), "missing blame should fail");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode missing blame refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("missing blame should emit JSON envelope");
    assert_eq!(envelope["kind"], "blame_file_not_found");
    assert!(
        envelope["error"].as_str().is_some_and(|error| error
            .contains("File 'missing.txt' not found in state")
            && error.contains("Unsafe:")
            && error.contains("Would change:")
            && error.contains("Preserved:")
            && error.contains("Primary recovery:")),
        "missing blame should include typed recovery detail: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle show")),
        "missing blame hint should name state inspection: {stderr}"
    );
}

#[test]
fn freshly_initialized_repo_reports_clean_health() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let text = heddle(&["--output", "text", "status"], Some(temp.path())).unwrap();
    assert!(
        text.contains("Health: clean"),
        "a fresh init should be healthy, not 'needs_attention': {text}"
    );
    assert!(
        !text.contains("Next step:"),
        "a fresh init has nothing to recommend; the renderer should stay silent: {text}"
    );

    let json = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    assert!(
        json.contains(r#""thread_health":"clean""#),
        "fresh-init JSON should carry the same 'clean' health: {json}"
    );
    assert!(
        json.contains(r#""recommended_action":"""#),
        "fresh-init JSON should expose an empty recommended_action: {json}"
    );
}

/// Build a local bare git repo with `master` carrying `commits`
/// commits, suitable for `heddle clone` from a local path.
fn make_local_master_git_repo(parent: &std::path::Path, commits: usize) -> std::path::PathBuf {
    let bare = parent.join("origin.git");
    let repo = gix::init_bare(&bare).expect("init bare origin");
    let mut parent_oid: Option<gix::hash::ObjectId> = None;
    for i in 0..commits {
        let blob = repo
            .write_blob(format!("content {i}\n").as_bytes())
            .expect("write blob")
            .detach();
        let empty = repo.empty_tree().id;
        let mut editor = repo.edit_tree(empty).expect("edit tree");
        editor
            .upsert(
                format!("f{i}.txt"),
                gix::object::tree::EntryKind::Blob,
                blob,
            )
            .expect("add file");
        let tree = editor.write().expect("write tree").detach();
        let parents = parent_oid.map(|p| vec![p]).unwrap_or_default();
        let commit = git_commit_with_tree(
            &repo,
            Some("refs/heads/master"),
            tree,
            &format!("c{i}"),
            &parents,
        );
        parent_oid = Some(commit);
    }
    // Honour the remote default branch so `heddle clone` picks `master`.
    git_set_reference(&repo, "HEAD", parent_oid.expect("at least one commit"));
    std::fs::write(bare.join("HEAD"), "ref: refs/heads/master\n")
        .expect("pin remote HEAD to master");
    bare
}

#[test]
fn bridge_git_import_after_clone_reports_commits_not_zero() {
    // heddle#147: rerunning `bridge git import --ref master --path .`
    // after `heddle clone` used to land at `commits_imported: 0` even
    // though every commit on master had been imported during clone —
    // visually indistinguishable from "your import did nothing".
    // After the fix, `commits_imported` reports commits walked (matching
    // `bridge git ingest`), `states_created` carries the dedup story,
    // and an `already_in_sync` flag tags the no-op case so callers can
    // render the right thing.
    let temp = TempDir::new().unwrap();
    let bare = make_local_master_git_repo(temp.path(), 3);
    let work = temp.path().join("work");

    heddle(
        &[
            "clone",
            bare.to_str().expect("origin path utf8"),
            work.to_str().expect("work path utf8"),
        ],
        Some(temp.path()),
    )
    .expect("heddle clone should succeed");

    let json = heddle(
        &[
            "--output", "json", "bridge", "git", "import", "--ref", "master", "--path", ".",
        ],
        Some(&work),
    )
    .expect("rerun bridge git import");
    let parsed: Value = serde_json::from_str(&json).expect("import JSON parses");
    assert_eq!(
        parsed["commits_imported"], 3,
        "commits_imported should report walked commits, not just new states: {json}"
    );
    assert_eq!(
        parsed["states_created"], 0,
        "no new heddle states should be created on a re-import: {json}"
    );
    assert_eq!(
        parsed["already_in_sync"], true,
        "already_in_sync should flag the no-op case: {json}"
    );
    assert_eq!(parsed["branches_synced"], 1);

    let text = heddle(
        &[
            "--output", "text", "bridge", "git", "import", "--ref", "master", "--path", ".",
        ],
        Some(&work),
    )
    .expect("rerun import text");
    assert!(
        text.contains("already in sync"),
        "text output should call out that the import was a no-op: {text}"
    );
}

#[test]
fn bridge_git_status_recommendation_runs_cleanly_after_clone() {
    // heddle#148: the recommended-action chain from `bridge git status`
    // used to dead-end at `heddle sync`. After clone, the bridge is in
    // sync (no missing branches) — the import_hint must be absent.
    // This is the structural side of the chain: status doesn't try to
    // drive the operator into a verb that errors.
    let temp = TempDir::new().unwrap();
    let bare = make_local_master_git_repo(temp.path(), 2);
    let work = temp.path().join("work");

    heddle(
        &[
            "clone",
            bare.to_str().expect("origin path utf8"),
            work.to_str().expect("work path utf8"),
        ],
        Some(temp.path()),
    )
    .expect("heddle clone");

    let json = heddle(
        &["--output", "json", "bridge", "git", "status"],
        Some(&work),
    )
    .expect("bridge git status JSON");
    let parsed: Value = serde_json::from_str(&json).expect("status JSON parses");
    assert!(
        parsed["git_overlay_import_hint"].is_null(),
        "bridge git status should report no missing branches after clone: {json}"
    );
}

#[test]
fn bridge_git_conflict_message_points_at_runnable_verbs() {
    // heddle#148: the divergence error used to suggest `heddle sync`,
    // which fails on a freshly-cloned overlay because the operator
    // thread has no metadata in the thread manager. The new message
    // must NOT mention `heddle sync` as the recovery, and must NOT
    // recommend `heddle bridge git sync` as a generic reconcile: sync
    // uses `PreviousValue::Any` when writing the Git branch ref
    // (see `sync_track_to_branch` in `crates/cli/src/bridge/git_sync.rs`)
    // so following that advice in a divergence drops branch-only
    // commits. The message must instead present both directional
    // escape hatches (Git-wins via `thread drop --delete-thread`,
    // Heddle-wins via `git branch -D`) so the operator picks
    // explicitly.
    use std::path::PathBuf;
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("bridge")
        .join("git_import.rs");
    let body =
        std::fs::read_to_string(&src).unwrap_or_else(|err| panic!("read {}: {err}", src.display()));
    let conflict_block = body
        .split("differs from branch")
        .nth(1)
        .expect("conflict format string carries 'differs from branch'");
    let conflict_block = conflict_block
        .split("));")
        .next()
        .expect("conflict block terminates");
    assert!(
        !conflict_block.contains("`heddle sync`"),
        "conflict message must not point at `heddle sync` — it errors on a \
         freshly-cloned git-overlay repo (#148): {conflict_block}"
    );
    assert!(
        !conflict_block.contains("heddle bridge git sync"),
        "conflict message must not recommend `heddle bridge git sync` for \
         divergent recovery — sync force-writes the Git branch via \
         PreviousValue::Any and would drop branch-only commits: \
         {conflict_block}"
    );
    assert!(
        conflict_block.contains("--delete-thread"),
        "conflict message should offer the Git-wins escape hatch \
         (`heddle thread drop --delete-thread`): {conflict_block}"
    );
    assert!(
        conflict_block.contains("git branch -D"),
        "conflict message should offer the Heddle-wins escape hatch \
         (`git branch -D`): {conflict_block}"
    );
}

#[test]
fn bridge_git_import_schema_declares_already_in_sync() {
    // heddle#147 added `already_in_sync: bool` to the JSON output of
    // `bridge git import`. The schema contract surfaced via
    // `heddle schemas "bridge git import"` must list the field, or
    // automation that validates against the schema will reject the
    // new payload shape.
    let schema = heddle(&["schemas", "bridge git import"], None)
        .expect("heddle schemas \"bridge git import\"");
    let parsed: Value = serde_json::from_str(&schema).expect("schema parses");
    let props = parsed["properties"]
        .as_object()
        .expect("schema has properties");
    assert!(
        props.contains_key("already_in_sync"),
        "BridgeImportSchema must declare `already_in_sync`: {schema}"
    );
    assert_eq!(
        props["already_in_sync"]["type"], "boolean",
        "`already_in_sync` must be a boolean: {schema}"
    );
}

#[test]
fn bridge_git_sync_after_clone_reports_zero_imported() {
    // heddle#147 made the import walker count every walked commit in
    // `commits_imported`. `bridge git sync` re-uses the importer, so
    // a no-op sync of an already-synced overlay used to report the
    // full walked history as `commits_imported` — exactly the signal
    // operators rely on sync to suppress. Sync must keep its
    // `commits_imported` scoped to commits that produced a new
    // heddle state on this run.
    let temp = TempDir::new().unwrap();
    let bare = make_local_master_git_repo(temp.path(), 3);
    let work = temp.path().join("work");

    heddle(
        &[
            "clone",
            bare.to_str().expect("origin path utf8"),
            work.to_str().expect("work path utf8"),
        ],
        Some(temp.path()),
    )
    .expect("heddle clone");

    let json = heddle(&["--output", "json", "bridge", "git", "sync"], Some(&work))
        .expect("bridge git sync JSON");
    let parsed: Value = serde_json::from_str(&json).expect("sync JSON parses");
    assert_eq!(
        parsed["commits_imported"], 0,
        "no-op sync should report zero newly-imported commits, not the \
         walked history: {json}"
    );

    let text = heddle(&["--output", "text", "bridge", "git", "sync"], Some(&work))
        .expect("bridge git sync text");
    assert!(
        text.contains("imported: 0 commits") || text.contains("imported: 0"),
        "text output should also report zero imported on a no-op sync: {text}"
    );
}
