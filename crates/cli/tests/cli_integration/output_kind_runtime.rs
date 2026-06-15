// SPDX-License-Identifier: Apache-2.0
//! Runtime `output_kind` coverage for verbs the catalog lint can't
//! invoke generically. The lint at [`super::output_kind_invariant`]
//! walks every swept verb that runs against a vanilla `heddle init`
//! fixture and asserts the emitted JSON carries the expected
//! discriminator. Several heddle#272 verbs need richer fixtures —
//! captured states, redactions, stashes, annotations, bisect sessions
//! — and live here.
//!
//! Each test asserts the catalog's wire contract: the JSON payload
//! parses, carries `output_kind` set to the snake-cased verb path,
//! and (where the sweep introduced an envelope shape) preserves the
//! pre-existing fields agents already key off.

use std::fs;

use cli::cli::commands::{build_command_catalog, operator_envelope_verbs};
use repo::{OutputFormat, RepoConfig};
use serde_json::Value;
use tempfile::TempDir;

use super::{heddle, heddle_output};

/// Init a repo, write a tracked file, capture one state.
fn init_and_capture() -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    heddle(&["init"], Some(temp.path())).expect("heddle init");
    fs::write(temp.path().join("main.rs"), "fn main() {}\n").expect("seed main.rs");
    heddle(&["capture", "-m", "seed"], Some(temp.path())).expect("seed capture");
    temp
}

/// Capture a second state on top of the seeded one so `HEAD~1` resolves.
fn capture_second(temp: &TempDir) {
    fs::write(
        temp.path().join("main.rs"),
        "fn main() { println!(\"hi\"); }\n",
    )
    .expect("modify main.rs");
    heddle(&["capture", "-m", "second"], Some(temp.path())).expect("second capture");
}

/// Run heddle with `--output json` and parse the first stdout line as JSON.
fn heddle_json(args: &[&str], temp: &TempDir) -> Value {
    let mut argv: Vec<&str> = vec!["--output", "json"];
    argv.extend(args.iter().copied());
    let stdout = heddle(&argv, Some(temp.path())).unwrap_or_else(|err| {
        panic!("heddle {argv:?} failed: {err}");
    });
    if let Ok(value) = serde_json::from_str(stdout.trim()) {
        return value;
    }
    let line = stdout
        .lines()
        .next()
        .unwrap_or_else(|| panic!("heddle {argv:?} produced no stdout"));
    serde_json::from_str(line)
        .unwrap_or_else(|err| panic!("heddle {argv:?} stdout not JSON: {err}\n  line: {line}"))
}

fn heddle_stdout_json_allow_failure(args: &[&str], temp: &TempDir) -> Value {
    let mut argv: Vec<&str> = vec!["--output", "json"];
    argv.extend(args.iter().copied());
    let output = heddle_output(&argv, Some(temp.path()))
        .unwrap_or_else(|err| panic!("heddle {argv:?} failed to run: {err}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_else(|| {
            panic!(
                "heddle {argv:?} produced no JSON stdout: status={:?} stderr={}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            )
        });
    serde_json::from_str(line)
        .unwrap_or_else(|err| panic!("heddle {argv:?} stdout line not JSON: {err}\n  line: {line}"))
}

fn assert_output_kind(value: &Value, expected: &str) {
    assert_eq!(
        value.get("output_kind").and_then(|v| v.as_str()),
        Some(expected),
        "expected output_kind={expected}, got payload: {value}"
    );
}

fn assert_not_output_kind(value: &Value, disallowed: &[&str]) {
    let actual = value
        .get("output_kind")
        .and_then(|v| v.as_str())
        .unwrap_or("<missing>");
    assert!(
        !disallowed.contains(&actual),
        "payload used disallowed output_kind={actual}: {value}"
    );
}

fn schema_ref<'a>(root: &'a Value, schema: &'a Value) -> &'a Value {
    let Some(reference) = schema.get("$ref").and_then(|value| value.as_str()) else {
        return schema;
    };
    reference
        .strip_prefix("#/$defs/")
        .or_else(|| reference.strip_prefix("#/definitions/"))
        .and_then(|name| {
            root.get("$defs")
                .or_else(|| root.get("definitions"))
                .and_then(|defs| defs.get(name))
        })
        .unwrap_or_else(|| panic!("schema reference {reference:?} resolves"))
}

fn schema_property_accepts(root: &Value, schema: &Value, value: &Value) -> Result<(), String> {
    let schema = schema_ref(root, schema);
    if let Some(enum_values) = schema.get("enum").and_then(|value| value.as_array())
        && !enum_values.contains(value)
    {
        return Err(format!("value {value} is not in enum {enum_values:?}"));
    }
    if let Some(const_value) = schema.get("const")
        && const_value != value
    {
        return Err(format!("value {value} does not match const {const_value}"));
    }
    Ok(())
}

fn schema_accepts_payload(root: &Value, schema: &Value, payload: &Value) -> Result<(), String> {
    let schema = schema_ref(root, schema);
    let payload_object = payload
        .as_object()
        .ok_or_else(|| format!("payload is not an object: {payload}"))?;

    if let Some(required) = schema.get("required").and_then(|value| value.as_array()) {
        for field in required.iter().filter_map(|value| value.as_str()) {
            if !payload_object.contains_key(field) {
                return Err(format!("payload is missing required field {field:?}"));
            }
        }
    }

    if let Some(properties) = schema.get("properties").and_then(|value| value.as_object()) {
        for (field, property_schema) in properties {
            if let Some(value) = payload_object.get(field) {
                schema_property_accepts(root, property_schema, value)
                    .map_err(|err| format!("property {field:?} rejected payload: {err}"))?;
            }
        }
    }

    if let Some(branches) = schema.get("anyOf").and_then(|value| value.as_array()) {
        let mut errors = Vec::new();
        for branch in branches {
            match schema_accepts_payload(root, branch, payload) {
                Ok(()) => return Ok(()),
                Err(err) => errors.push(err),
            }
        }
        return Err(format!("payload matched no anyOf branch: {errors:?}"));
    }

    if let Some(subschemas) = schema.get("allOf").and_then(|value| value.as_array()) {
        let mut errors = Vec::new();
        for subschema in subschemas {
            if let Err(err) = schema_accepts_payload(root, subschema, payload) {
                errors.push(err);
            }
        }
        if !errors.is_empty() {
            return Err(format!("payload failed allOf subschema(s): {errors:?}"));
        }
    }

    Ok(())
}

fn assert_schema_accepts_payload(schema: &Value, payload: &Value) {
    schema_accepts_payload(schema, schema, payload)
        .unwrap_or_else(|err| panic!("published schema rejected payload: {err}\n{payload}"));
}

#[test]
fn switch_emits_output_kind_with_target_metadata() {
    let temp = init_and_capture();
    capture_second(&temp);
    let value = heddle_json(&["switch", "HEAD~1"], &temp);
    assert_output_kind(&value, "thread_switch");
    assert!(
        value.get("target").and_then(|v| v.as_str()).is_some(),
        "switch JSON must carry `target` state id: {value}"
    );
    assert!(
        value
            .get("message")
            .and_then(|v| v.as_str())
            .is_some_and(|m| m.starts_with("Now at: ")),
        "switch JSON must carry the `message` field: {value}"
    );
}

#[test]
fn revert_no_commit_emits_output_kind_without_change_id() {
    let temp = init_and_capture();
    capture_second(&temp);
    let value = heddle_json(&["revert", "HEAD", "--no-commit"], &temp);
    assert_output_kind(&value, "revert");
    assert!(
        value["change_id"].is_null(),
        "revert --no-commit must leave change_id null: {value}"
    );
    assert!(
        value
            .get("files_affected")
            .and_then(|v| v.as_array())
            .is_some_and(|arr| !arr.is_empty()),
        "revert must report files_affected: {value}"
    );
}

#[test]
fn revert_commit_emits_output_kind_with_new_change_id() {
    let temp = init_and_capture();
    capture_second(&temp);
    let value = heddle_json(&["revert", "HEAD"], &temp);
    assert_output_kind(&value, "revert");
    assert!(
        value
            .get("change_id")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty()),
        "revert (committed) must carry the new state's change_id: {value}"
    );
}

#[test]
fn clean_dry_run_emits_output_kind() {
    let temp = init_and_capture();
    // Add an untracked file so the removed list is non-trivial.
    fs::write(temp.path().join("stray.txt"), "junk\n").expect("write stray");
    let value = heddle_json(&["clean", "--dry-run"], &temp);
    assert_output_kind(&value, "clean");
    assert_eq!(
        value.get("dry_run").and_then(|v| v.as_bool()),
        Some(true),
        "clean --dry-run must report dry_run=true: {value}"
    );
    assert!(
        value
            .get("removed")
            .and_then(|v| v.as_array())
            .is_some_and(|arr| arr
                .iter()
                .any(|entry| entry.as_str().is_some_and(|s| s.contains("stray.txt")))),
        "clean --dry-run must list the would-remove paths: {value}"
    );
}

#[test]
fn cherry_pick_no_commit_emits_output_kind() {
    let temp = init_and_capture();
    fs::write(temp.path().join("feature.txt"), "added\n").expect("write feature");
    heddle(&["capture", "-m", "feature"], Some(temp.path())).expect("feature capture");

    // Resolve the feature state's id (HEAD), then move back to the seed.
    let head = heddle_json(&["log", "--limit", "1"], &temp);
    let feature_id = head["states"][0]["change_id"]
        .as_str()
        .expect("log --output json must expose change_id")
        .to_string();
    heddle(&["switch", "HEAD~1"], Some(temp.path())).expect("goto back to seed");

    let value = heddle_json(&["cherry-pick", &feature_id, "--no-commit"], &temp);
    assert_output_kind(&value, "cherry_pick");
    assert_eq!(value["status"].as_str(), Some("applied"));
    assert_eq!(value["no_commit"].as_bool(), Some(true));
    assert_eq!(
        value["commit"].as_str(),
        Some(feature_id.as_str()),
        "cherry-pick must echo the source commit id: {value}"
    );
}

#[test]
fn cherry_pick_commit_emits_output_kind_with_new_commit() {
    let temp = init_and_capture();
    fs::write(temp.path().join("feature.txt"), "added\n").expect("write feature");
    heddle(&["capture", "-m", "feature"], Some(temp.path())).expect("feature capture");
    let head = heddle_json(&["log", "--limit", "1"], &temp);
    let feature_id = head["states"][0]["change_id"]
        .as_str()
        .expect("log JSON change_id")
        .to_string();
    heddle(&["switch", "HEAD~1"], Some(temp.path())).expect("goto back to seed");

    let value = heddle_json(&["cherry-pick", &feature_id], &temp);
    assert_output_kind(&value, "cherry_pick");
    assert_eq!(value["status"].as_str(), Some("committed"));
    assert_eq!(value["commit"].as_str(), Some(feature_id.as_str()));
    assert!(
        value
            .get("new_commit")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty()),
        "cherry-pick (committed) must report new_commit: {value}"
    );
}

#[test]
fn thread_operator_envelopes_emit_approved_thread_kinds() {
    let temp = init_and_capture();
    heddle(&["thread", "create", "side"], Some(temp.path())).expect("thread create side");

    let refresh = heddle_json(&["thread", "refresh", "side"], &temp);
    assert_output_kind(&refresh, "thread_refresh");
    assert_not_output_kind(&refresh, &["thread"]);
    assert_eq!(refresh["action"].as_str(), Some("thread_refresh"));

    let resolve = heddle_json(&["thread", "resolve", "side"], &temp);
    assert_output_kind(&resolve, "thread_resolve");
    assert_not_output_kind(&resolve, &["resolve"]);
    assert_eq!(resolve["action"].as_str(), Some("thread_resolve"));

    let drop = heddle_json(&["thread", "drop", "side"], &temp);
    assert_output_kind(&drop, "thread_drop");
    assert_not_output_kind(&drop, &["thread"]);
    assert_eq!(drop["action"].as_str(), Some("thread_drop"));
}

#[test]
fn thread_promote_and_cleanup_emit_approved_thread_kinds() {
    let temp = init_and_capture();
    heddle(&["start", "promo"], Some(temp.path())).expect("start promo thread");

    let promote = heddle_json(&["thread", "promote", "promo"], &temp);
    assert_output_kind(&promote, "thread_promote");
    assert_not_output_kind(&promote, &["thread"]);
    assert_eq!(promote["action"].as_str(), Some("thread_promote"));

    let cleanup = heddle_json(&["thread", "cleanup", "--merged", "--dry-run"], &temp);
    assert_output_kind(&cleanup, "thread_cleanup");
    assert_not_output_kind(&cleanup, &["thread.cleanup"]);
    assert_eq!(cleanup["action"].as_str(), Some("thread_cleanup"));
}

#[test]
fn thread_rename_and_delete_advertised_thread_kinds_are_runtime_truths() {
    let temp = init_and_capture();
    heddle(&["thread", "create", "side"], Some(temp.path())).expect("thread create side");

    let rename = heddle_json(&["thread", "rename", "side", "renamed"], &temp);
    assert_output_kind(&rename, "thread_rename");

    let delete = heddle_json(&["thread", "drop", "renamed", "--delete-thread"], &temp);
    assert_output_kind(&delete, "thread_drop");
}

#[test]
fn show_and_thread_show_emit_distinct_output_kinds() {
    let temp = init_and_capture();

    let show = heddle_json(&["show", "HEAD"], &temp);
    assert_output_kind(&show, "show");

    let thread = heddle_json(&["thread", "show"], &temp);
    assert_output_kind(&thread, "thread_show");
}

#[test]
fn show_and_thread_show_schemas_accept_payloads() {
    let temp = init_and_capture();
    let show_schema: Value = serde_json::from_str(
        &heddle(&["schemas", "show"], Some(temp.path())).expect("schemas show"),
    )
    .expect("schemas show emits JSON schema");
    let thread_show_schema: Value = serde_json::from_str(
        &heddle(&["schemas", "thread", "show"], Some(temp.path())).expect("schemas thread show"),
    )
    .expect("schemas thread show emits JSON schema");

    let show = heddle_json(&["show", "HEAD"], &temp);
    assert_output_kind(&show, "show");
    assert_schema_accepts_payload(&show_schema, &show);

    let thread = heddle_json(&["thread", "show"], &temp);
    assert_output_kind(&thread, "thread_show");
    assert_schema_accepts_payload(&thread_show_schema, &thread);
}

fn init_rebase_replay_fixture() -> TempDir {
    let temp = init_and_capture();
    heddle(&["thread", "create", "feature"], Some(temp.path())).expect("thread create feature");
    heddle(&["thread", "switch", "feature"], Some(temp.path())).expect("switch feature");
    fs::write(temp.path().join("feat.txt"), "feature work\n").expect("write feature file");
    heddle(&["capture", "-m", "feature"], Some(temp.path())).expect("feature capture");
    heddle(&["thread", "switch", "main"], Some(temp.path())).expect("switch main");
    fs::write(temp.path().join("main.txt"), "main work\n").expect("write main file");
    heddle(&["capture", "-m", "main"], Some(temp.path())).expect("main capture");
    temp
}

fn set_repo_output_format_json(temp: &TempDir) {
    let config_path = temp.path().join(".heddle").join("config.toml");
    let mut config = RepoConfig::load(&config_path).expect("load repo config");
    config.output.format = Some(OutputFormat::Json);
    config.save(&config_path).expect("save repo config");
}

fn init_active_merge_fixture() -> TempDir {
    let temp = init_and_capture();
    heddle(&["thread", "create", "feature"], Some(temp.path())).expect("thread create feature");
    heddle(&["thread", "switch", "feature"], Some(temp.path())).expect("switch feature");
    fs::write(temp.path().join("main.rs"), "fn main() { feature(); }\n")
        .expect("write feature conflict");
    heddle(&["capture", "-m", "feature"], Some(temp.path())).expect("feature capture");
    heddle(&["thread", "switch", "main"], Some(temp.path())).expect("switch main");
    fs::write(temp.path().join("main.rs"), "fn main() { mainline(); }\n")
        .expect("write main conflict");
    heddle(&["capture", "-m", "main"], Some(temp.path())).expect("main capture");
    heddle(&["thread", "switch", "feature"], Some(temp.path())).expect("switch feature again");

    let output =
        heddle_output(&["merge", "main"], Some(temp.path())).expect("conflicted merge runs");
    assert!(
        temp.path().join(".heddle").join("MERGE_STATE").exists(),
        "merge fixture must leave active MERGE_STATE: status={:?} stdout={} stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    temp
}

fn init_active_rebase_fixture() -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    heddle(&["init"], Some(temp.path())).expect("heddle init");
    fs::write(temp.path().join("conflict.txt"), "base\n").expect("write base");
    heddle(&["capture", "-m", "base"], Some(temp.path())).expect("base capture");

    heddle(&["thread", "create", "feature"], Some(temp.path())).expect("thread create feature");
    heddle(&["thread", "switch", "feature"], Some(temp.path())).expect("switch feature");
    fs::write(temp.path().join("conflict.txt"), "feature\n").expect("write feature");
    heddle(&["capture", "-m", "feature"], Some(temp.path())).expect("feature capture");

    heddle(&["thread", "switch", "main"], Some(temp.path())).expect("switch main");
    fs::write(temp.path().join("conflict.txt"), "main\n").expect("write main");
    heddle(&["capture", "-m", "main"], Some(temp.path())).expect("main capture");

    let output =
        heddle_output(&["rebase", "feature"], Some(temp.path())).expect("conflicted rebase runs");
    assert!(
        temp.path().join(".heddle").join("REBASE_STATE").exists(),
        "rebase fixture must leave active REBASE_STATE: status={:?} stdout={} stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    temp
}

#[derive(Clone, Copy)]
enum RecoveryFixture {
    Idle,
    ActiveMerge,
    ActiveRebase,
}

impl RecoveryFixture {
    fn init(self) -> TempDir {
        match self {
            Self::Idle => init_and_capture(),
            Self::ActiveMerge => init_active_merge_fixture(),
            Self::ActiveRebase => init_active_rebase_fixture(),
        }
    }

    fn expected_action(self, verb: &str) -> &str {
        match (self, verb) {
            (Self::Idle, _) | (_, "sync") => verb,
            (Self::ActiveMerge, _) => "merge",
            (Self::ActiveRebase, _) => "rebase",
        }
    }
}

fn catalog_operator_envelope_output_kinds() -> Vec<(String, String)> {
    let catalog = build_command_catalog();
    operator_envelope_verbs()
        .into_iter()
        .map(|display| {
            let entry = catalog
                .commands
                .iter()
                .find(|entry| entry.display == display)
                .unwrap_or_else(|| panic!("operator envelope verb `{display}` is cataloged"));
            let output_kind = entry
                .json_discriminators
                .iter()
                .find(|discriminator| discriminator.field == "output_kind")
                .unwrap_or_else(|| {
                    panic!("operator envelope verb `{display}` declares output_kind")
                })
                .value
                .clone();
            (display, output_kind)
        })
        .collect()
}

#[test]
fn recovery_path_family_output_kind_matches_invoked_verb() {
    let operator_verbs = catalog_operator_envelope_output_kinds();
    assert!(
        operator_verbs.iter().any(|(display, _)| display == "sync"),
        "catalog-driven operator envelope sweep must include `sync`: {operator_verbs:?}"
    );

    for fixture in [
        RecoveryFixture::Idle,
        RecoveryFixture::ActiveMerge,
        RecoveryFixture::ActiveRebase,
    ] {
        for (verb, output_kind) in &operator_verbs {
            if matches!(fixture, RecoveryFixture::Idle) && verb == "sync" {
                continue;
            }
            let temp = fixture.init();
            let args = verb.split_whitespace().collect::<Vec<_>>();
            let value = heddle_stdout_json_allow_failure(&args, &temp);
            assert_output_kind(&value, output_kind);
            assert_eq!(
                value["action"].as_str(),
                Some(fixture.expected_action(verb)),
                "{verb} should keep the operation action while output_kind stays tied to the invoked command: {value}"
            );
            if !matches!(fixture, RecoveryFixture::Idle) {
                assert_not_output_kind(&value, &["merge", "rebase"]);
            }
        }
    }
}

#[test]
fn rebase_json_records_emit_progress_output_kind() {
    let temp = init_rebase_replay_fixture();

    let output = heddle(
        &["--output", "json", "rebase", "feature"],
        Some(temp.path()),
    )
    .expect("rebase feature");
    let lines = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    assert!(
        !lines.is_empty(),
        "rebase JSON output should emit at least one record"
    );
    let mut statuses = Vec::new();
    for line in lines {
        let value: Value = serde_json::from_str(line)
            .unwrap_or_else(|err| panic!("rebase line not JSON: {err}\n  line: {line}"));
        assert_output_kind(&value, "rebase_progress");
        statuses.push(
            value["status"]
                .as_str()
                .unwrap_or_else(|| panic!("rebase line missing status: {value}"))
                .to_string(),
        );
    }
    assert_eq!(
        statuses,
        vec!["started", "applying", "completed"],
        "fixture must exercise replay JSONL, not the fast-forward path"
    );
}

#[test]
fn rebase_repo_json_config_without_output_flag_stays_text() {
    let temp = init_rebase_replay_fixture();
    set_repo_output_format_json(&temp);

    let output = heddle(&["rebase", "feature"], Some(temp.path())).expect("rebase feature");
    assert!(
        output.contains("Rebasing ")
            && output.contains("Applying ")
            && output.contains("Rebase completed"),
        "plain rebase under repo output.format=json should emit text, got: {output}"
    );
    assert!(
        output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .all(|line| serde_json::from_str::<Value>(line).is_err()),
        "plain rebase under repo output.format=json must not emit JSONL: {output}"
    );
}

#[test]
fn stash_show_emits_output_kind() {
    let temp = init_and_capture();
    // Dirty the worktree so stash push has something to capture.
    fs::write(temp.path().join("main.rs"), "fn main() { /* tweak */ }\n").expect("modify main.rs");
    heddle(&["stash", "push", "-m", "wip"], Some(temp.path())).expect("stash push");

    let value = heddle_json(&["stash", "show"], &temp);
    assert_output_kind(&value, "stash_show");
    // The stash modified `main.rs`; the diff against the stash parent
    // must surface it as a modified path. Catches the case where the
    // sweep wired `output_kind` but broke the diff fields underneath.
    assert!(
        value
            .get("modified")
            .and_then(|v| v.as_array())
            .is_some_and(|arr| arr.iter().any(|p| p.as_str() == Some("main.rs"))),
        "stash show must list main.rs as modified: {value}"
    );
}

#[test]
fn redact_apply_show_emit_output_kind() {
    let temp = init_and_capture();
    // Reuse main.rs as the redaction target; redact apply doesn't
    // care that the content isn't a "real" secret.
    let log = heddle_json(&["log", "--limit", "1"], &temp);
    let state = log["states"][0]["change_id"]
        .as_str()
        .expect("log JSON change_id")
        .to_string();

    let apply = heddle_json(
        &[
            "redact", "apply", &state, "--path", "main.rs", "--reason", "test",
        ],
        &temp,
    );
    assert_output_kind(&apply, "redact_apply");
    let redaction_id = apply["redaction_id"]
        .as_str()
        .expect("redact apply must carry redaction_id")
        .to_string();

    let show = heddle_json(&["redact", "show", &redaction_id], &temp);
    assert_output_kind(&show, "redact_show");
    assert_eq!(show["redaction_id"].as_str(), Some(redaction_id.as_str()));
}

#[test]
fn redact_trust_add_and_remove_emit_output_kind() {
    let temp = init_and_capture();
    // Ed25519 public keys are 32 bytes / 64 hex chars; the trust
    // store accepts any well-formed hex without contacting a key
    // server. Using a deterministic test key keeps add+remove tied.
    let pubkey = "11".repeat(32);

    let add = heddle_json(
        &[
            "redact",
            "trust",
            "add",
            "--algorithm",
            "ed25519",
            "--public-key",
            &pubkey,
            "--label",
            "test-key",
        ],
        &temp,
    );
    assert_output_kind(&add, "redact_trust_add");
    // The envelope flattens TrustEntryOutput so the wire shape stays
    // compatible with PR #251's `trusted_keys` row format.
    assert_eq!(add["algorithm"].as_str(), Some("ed25519"));
    assert_eq!(add["public_key"].as_str(), Some(pubkey.as_str()));
    assert_eq!(add["label"].as_str(), Some("test-key"));

    let remove = heddle_json(&["redact", "trust", "remove", &pubkey], &temp);
    assert_output_kind(&remove, "redact_trust_remove");
    assert_eq!(remove["removed"].as_u64(), Some(1));
}

#[test]
fn purge_apply_emits_output_kind() {
    let temp = init_and_capture();
    let log = heddle_json(&["log", "--limit", "1"], &temp);
    let state = log["states"][0]["change_id"]
        .as_str()
        .expect("log JSON change_id")
        .to_string();
    heddle(
        &[
            "redact", "apply", &state, "--path", "main.rs", "--reason", "test",
        ],
        Some(temp.path()),
    )
    .expect("redact apply");

    let value = heddle_json(
        &[
            "redact", "purge", "apply", &state, "--path", "main.rs", "--force",
        ],
        &temp,
    );
    assert_output_kind(&value, "purge_apply");
    assert!(
        value.get("blob").and_then(|v| v.as_str()).is_some(),
        "purge apply must echo blob id: {value}"
    );
}

#[test]
fn context_set_get_history_audit_check_emit_output_kind() {
    let temp = init_and_capture();

    let set = heddle_json(
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
        &temp,
    );
    assert_output_kind(&set, "context_set");
    assert_eq!(set["target"].as_str(), Some("main.rs"));

    let get = heddle_json(&["context", "get", "--path", "main.rs"], &temp);
    assert_output_kind(&get, "context_get");
    let annotation_id = get["annotations"][0]["annotation_id"]
        .as_str()
        .expect("context get must surface annotation_id")
        .to_string();

    let history = heddle_json(&["context", "history", &annotation_id], &temp);
    assert_output_kind(&history, "context_history");
    assert_eq!(
        history["annotation_id"].as_str(),
        Some(annotation_id.as_str())
    );

    let edit = heddle_json(
        &[
            "context",
            "edit",
            &annotation_id,
            "-m",
            "refined entry point",
        ],
        &temp,
    );
    assert_output_kind(&edit, "context_edit");
    assert_eq!(edit["annotation_id"].as_str(), Some(annotation_id.as_str()));
    assert_eq!(edit["revision_count"].as_u64(), Some(2));

    let supersede = heddle_json(
        &[
            "context",
            "supersede",
            &annotation_id,
            "--path",
            "main.rs",
            "--scope",
            "file",
            "--kind",
            "rationale",
            "-m",
            "fully rewritten guidance",
        ],
        &temp,
    );
    assert_output_kind(&supersede, "context_supersede");
    assert_eq!(supersede["replacement_target"].as_str(), Some("main.rs"));

    let check = heddle_json(&["context", "check"], &temp);
    assert_output_kind(&check, "context_check");

    let audit = heddle_json(&["context", "audit"], &temp);
    assert_output_kind(&audit, "context_audit");
    // After set→edit→supersede there's exactly one logical
    // annotation but two active+superseded rows; the audit
    // counter must reflect both.
    assert!(
        audit["annotations"].as_u64().is_some_and(|n| n >= 2),
        "context audit must count active+superseded rows: {audit}"
    );

    let suggest = heddle_json(&["context", "suggest"], &temp);
    assert_output_kind(&suggest, "context_suggest");
    assert!(
        suggest.get("items").and_then(|v| v.as_array()).is_some(),
        "context suggest envelope must carry an `items` array: {suggest}"
    );

    let rm = heddle_json(&["context", "rm", "--path", "main.rs", "--all"], &temp);
    assert_output_kind(&rm, "context_rm");
    assert_eq!(rm["removed"].as_bool(), Some(true));
}

#[test]
fn context_list_envelope_wraps_items_for_empty_and_populated() {
    // Empty list — the `context_root.is_none()` early return path
    // (already covered by the lint test) emits the envelope shape.
    let empty_temp = init_and_capture();
    let empty = heddle_json(&["context", "list"], &empty_temp);
    assert_output_kind(&empty, "context_list");
    assert_eq!(
        empty["items"].as_array().map(|arr| arr.len()),
        Some(0),
        "empty context list must wrap as {{output_kind, items:[]}}: {empty}"
    );

    // Populated list — exercises the entry-collection branch in
    // `cmd_context_list` that builds the `items` Vec.
    let populated_temp = init_and_capture();
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
        Some(populated_temp.path()),
    )
    .expect("context set");
    let populated = heddle_json(&["context", "list"], &populated_temp);
    assert_output_kind(&populated, "context_list");
    let items = populated["items"]
        .as_array()
        .expect("populated context list items array");
    assert!(
        !items.is_empty(),
        "populated context list must surface items: {populated}"
    );
    // List rows MUST NOT repeat a per-row `output_kind`: the
    // `context_list` envelope owns the discriminator. A row that carried
    // its own `output_kind: "context_get"` would make consumers that
    // route recursively on the discriminator misclassify the list row as
    // a standalone `context get` payload (heddle#272 Codex r5 finding).
    assert!(
        items[0].get("output_kind").is_none(),
        "context list rows must not carry a nested output_kind: {populated}"
    );
    // The row still carries its substantive fields.
    assert!(
        items[0].get("target").and_then(|v| v.as_str()).is_some(),
        "context list row must keep its `target`: {populated}"
    );
    assert!(
        items[0]
            .get("annotations")
            .and_then(|v| v.as_array())
            .is_some(),
        "context list row must keep its `annotations` array: {populated}"
    );
}

#[test]
fn discuss_open_show_append_emit_output_kind() {
    let temp = init_and_capture();

    let open = heddle_json(&["discuss", "open", "main.rs", "main", "first turn"], &temp);
    assert_output_kind(&open, "discuss_open");
    let discussion_id = open["id"]
        .as_str()
        .expect("discuss open envelope must flatten the discussion `id`")
        .to_string();

    let append = heddle_json(
        &["discuss", "append", &discussion_id, "follow-up turn"],
        &temp,
    );
    assert_output_kind(&append, "discuss_append");
    assert_eq!(append["id"].as_str(), Some(discussion_id.as_str()));

    let show = heddle_json(&["discuss", "show", &discussion_id], &temp);
    assert_output_kind(&show, "discuss_show");
    assert_eq!(show["id"].as_str(), Some(discussion_id.as_str()));
    // Flattened `turns` field must surface both turns at the top
    // level — the envelope must not nest the discussion payload.
    assert_eq!(
        show["turns"].as_array().map(|arr| arr.len()).unwrap_or(0),
        2,
        "discuss show must flatten `turns` at the top level: {show}"
    );

    let resolve = heddle_json(
        &[
            "discuss",
            "resolve",
            &discussion_id,
            "--mode",
            "dismiss",
            "--reason",
            "not relevant",
        ],
        &temp,
    );
    assert_output_kind(&resolve, "discuss_resolve");
    assert_eq!(resolve["id"].as_str(), Some(discussion_id.as_str()));
}

#[test]
fn review_show_emits_output_kind() {
    let temp = init_and_capture();
    let value = heddle_json(&["review", "show", "HEAD"], &temp);
    assert_output_kind(&value, "review_show");
    assert!(
        value
            .get("change_id")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty()),
        "review show must surface change_id: {value}"
    );
}

#[test]
fn review_next_envelope_is_emitted_when_window_empty() {
    // Smoke test: `review next` against a fresh repo with no
    // pending review work hits the `None`-branch of the envelope —
    // that path is the one the lint test exercises generally, but
    // asserting the explicit `next: null` shape here pins the wire
    // contract that agents key off when there's nothing to review.
    let temp = init_and_capture();
    let value = heddle_json(&["review", "next"], &temp);
    assert_output_kind(&value, "review_next");
    assert!(
        value.get("next").is_some(),
        "review next must always emit a `next` field (null or object): {value}"
    );
}

#[test]
fn purge_list_envelope_includes_recent_apply() {
    let temp = init_and_capture();
    let log = heddle_json(&["log", "--limit", "1"], &temp);
    let state = log["states"][0]["change_id"].as_str().unwrap().to_string();
    heddle(
        &[
            "redact", "apply", &state, "--path", "main.rs", "--reason", "test",
        ],
        Some(temp.path()),
    )
    .expect("redact apply");
    heddle(
        &[
            "redact", "purge", "apply", &state, "--path", "main.rs", "--force",
        ],
        Some(temp.path()),
    )
    .expect("purge apply");

    let value = heddle_json(&["redact", "purge", "list"], &temp);
    assert_output_kind(&value, "purge_list");
    assert!(
        value["count"].as_u64().is_some_and(|n| n >= 1),
        "purge list after purge apply must show at least one entry: {value}"
    );
}
