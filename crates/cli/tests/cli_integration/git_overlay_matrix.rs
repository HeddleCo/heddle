// SPDX-License-Identifier: Apache-2.0
use super::*;

/// The `bisect` verb was removed in the whole-CLI consolidation (heddle#473),
/// but the operation-status layer still detects a lingering `BISECT_STATE`
/// file (from older binaries) as an in-progress Heddle operation. These tests
/// exercise that detection + the continue/abort/undo guardrails, so they seed
/// the state file directly instead of through the (now removed) verb.
fn seed_heddle_bisect_state(path: &std::path::Path) {
    // A real stale BISECT_STATE only ever exists inside an initialized Heddle
    // overlay, so bootstrap one first — opening a bare `.heddle` that holds
    // only the marker file is not a valid repository.
    heddle(&["init"], Some(path)).expect("heddle init for bisect fixture");
    std::fs::write(path.join(".heddle").join("BISECT_STATE"), "{}\n")
        .expect("seed BISECT_STATE");
}

fn init_git_repo_with_branch(path: &std::path::Path, branch: &str) {
    let status = Command::new("git")
        .arg("init")
        .current_dir(path)
        .status()
        .expect("git init should run");
    assert!(status.success(), "git init should succeed");

    let status = Command::new("git")
        .args(["config", "user.name", "Heddle Test"])
        .current_dir(path)
        .status()
        .expect("git config user.name should run");
    assert!(status.success());

    let status = Command::new("git")
        .args(["config", "user.email", "heddle@example.com"])
        .current_dir(path)
        .status()
        .expect("git config user.email should run");
    assert!(status.success());

    let status = Command::new("git")
        .args(["checkout", "-b", branch])
        .current_dir(path)
        .status()
        .expect("git checkout -b should run");
    assert!(status.success());
}

fn git(args: &[&str], path: &std::path::Path) {
    let status = Command::new("git")
        .args(args)
        .current_dir(path)
        .status()
        .unwrap_or_else(|err| panic!("git {:?} should run: {}", args, err));
    assert!(status.success(), "git {:?} should succeed", args);
}

fn git_stdout(path: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .unwrap_or_else(|err| panic!("git {:?} should run: {}", args, err));
    assert!(output.status.success(), "git {:?} should succeed", args);
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn mirror_git_stdout(path: &std::path::Path, args: &[&str]) -> String {
    git_stdout(&path.join(".heddle/git"), args)
}

fn git_status_short(path: &std::path::Path) -> String {
    git_stdout(path, &["status", "--short"])
}

fn git_ref_snapshot(path: &std::path::Path) -> String {
    format!(
        "HEAD {}\n{}",
        git_stdout(path, &["rev-parse", "HEAD"]),
        git_stdout(path, &["for-each-ref", "--format=%(refname) %(objectname)"])
    )
}

fn git_commit_all(path: &std::path::Path, message: &str) {
    git(&["add", "."], path);
    git(&["commit", "-m", message], path);
}

fn heddle_adopt(path: &std::path::Path) {
    heddle(&["adopt"], Some(path)).unwrap();
}

fn setup_diverged_imported_git_ref(path: &std::path::Path) {
    init_git_repo_with_branch(path, "main");
    std::fs::write(path.join("shared.txt"), "base\n").unwrap();
    std::fs::write(path.join("main-only.txt"), "main-only\n").unwrap();
    git_commit_all(path, "base");

    git(&["switch", "-c", "feature"], path);
    std::fs::write(path.join("shared.txt"), "feature edit\n").unwrap();
    std::fs::write(path.join("feature.txt"), "feature\n").unwrap();
    git_commit_all(path, "feature");

    git(&["switch", "main"], path);
    std::fs::write(path.join("main-only.txt"), "main edit\n").unwrap();
    std::fs::write(path.join("main2.txt"), "main file\n").unwrap();
    git_commit_all(path, "main");

    heddle_adopt(path);
}

fn setup_linear_imported_git_ref(path: &std::path::Path) {
    init_git_repo_with_branch(path, "main");
    std::fs::write(path.join("base.txt"), "base\n").unwrap();
    git_commit_all(path, "base");

    git(&["switch", "-c", "feature"], path);
    std::fs::write(path.join("feature.txt"), "feature\n").unwrap();
    git_commit_all(path, "feature");

    git(&["switch", "main"], path);
    heddle_adopt(path);
}

fn raw_git_preservation_action() -> &'static str {
    "heddle bridge git status"
}

fn json(cwd: &std::path::Path, args: &[&str]) -> Value {
    // Helper exists to parse JSON; explicit --output json is now
    // required (no more auto-on-pipe). Inject it if the caller
    // didn't already supply it so existing call sites Just Work.
    let mut full_args: Vec<&str> = Vec::with_capacity(args.len() + 2);
    if !args.iter().any(|arg| *arg == "json" || *arg == "text") {
        full_args.push("--output");
        full_args.push("json");
    }
    full_args.extend_from_slice(args);
    let output = heddle_output(&full_args, Some(cwd))
        .unwrap_or_else(|err| panic!("heddle {full_args:?}: {err}"));
    let stdout = std::str::from_utf8(&output.stdout).unwrap_or("");
    let stderr = std::str::from_utf8(&output.stderr).unwrap_or("");
    if output.status.success() || !stdout.trim().is_empty() {
        let parsed: Value = serde_json::from_str(stdout)
            .unwrap_or_else(|err| panic!("expected JSON for {:?}: {}", args, err));
        return inject_post_verification(cwd, args, parsed);
    }
    if args.contains(&"verify") {
        let envelope: Value = serde_json::from_str(stderr).unwrap_or_else(|err| {
            panic!(
                "expected verify JSON envelope for {:?}: {err}: {stderr}",
                args
            )
        });
        if envelope["kind"] == "verify_failed" {
            let mut verification = envelope["verification"].clone();
            if let Some(object) = verification.as_object_mut() {
                object.insert(
                    "output_kind".to_string(),
                    Value::String("verify".to_string()),
                );
                object.insert("clean".to_string(), Value::Bool(false));
            }
            return verification;
        }
    }
    panic!(
        "heddle {:?} failed: code={:?}\nstdout: {}\nstderr: {}",
        args,
        output.status.code(),
        stdout,
        stderr
    );
}

/// Mutation `--output json` replies no longer embed `verification`
/// (the verification-claim gate still consults it in-memory, but it
/// is omitted from the wire to keep mutation replies focused).
/// These integration tests pre-date that change and pattern-match
/// on `mutation["verification"]`; rather than rewrite every callsite
/// to issue a separate `heddle verify`, this helper performs that
/// follow-up call once and grafts the proof back onto the returned
/// value. Real consumers see the field omitted; the helper restores
/// it only for test ergonomics.
fn inject_post_verification(cwd: &std::path::Path, args: &[&str], mut value: Value) -> Value {
    let obj = match value.as_object_mut() {
        Some(obj) => obj,
        None => return value,
    };
    if obj.contains_key("verification") {
        return value;
    }
    // Skip for non-mutation reads where verification absence is intentional.
    if args.iter().any(|a| *a == "verify" || *a == "doctor") {
        return value;
    }
    // Try to fetch verify; if it fails (e.g. plain-git, no .heddle), give up
    // silently and return the value unmodified.
    let verify_out = match heddle_output(&["--output", "json", "verify"], Some(cwd)) {
        Ok(out) => out,
        Err(_) => return value,
    };
    let stream = if !verify_out.status.success() {
        verify_out.stderr
    } else {
        verify_out.stdout
    };
    let text = std::str::from_utf8(&stream).unwrap_or("");
    let parsed: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return value,
    };
    let verification = if parsed.get("kind") == Some(&Value::String("verify_failed".to_string())) {
        parsed.get("verification").cloned().unwrap_or(Value::Null)
    } else {
        // Clean verify flattens the proof; reconstruct as a nested
        // object by dropping verify's own wrapper keys.
        let mut obj_map = parsed
            .as_object()
            .cloned()
            .unwrap_or_default();
        obj_map.remove("output_kind");
        obj_map.remove("repository_label");
        obj_map.remove("repository_context");
        obj_map.remove("clean");
        Value::Object(obj_map)
    };
    obj.insert("verification".to_string(), verification);
    value
}

fn assert_operator_json_contract(parsed: &Value, output_kind: &str) {
    assert_eq!(parsed["output_kind"], output_kind, "{parsed}");
    for (action_field, template_field) in [
        ("next_action", "next_action_template"),
        ("recommended_action", "recommended_action_template"),
    ] {
        if parsed[action_field].is_null() {
            assert_eq!(parsed[template_field], Value::Null, "{parsed}");
            continue;
        }
        let action = parsed[action_field]
            .as_str()
            .unwrap_or_else(|| panic!("{action_field} should be string or null: {parsed}"));
        assert!(
            !action.trim().is_empty(),
            "{action_field} should serialize absent actions as null: {parsed}"
        );
        // Every valid action carries the canonical fillable template
        // (HeddleCo/heddle#254); the always-null `_argv` sibling was dropped.
        assert!(
            parsed[template_field]["argv_template"]
                .as_array()
                .is_some_and(|argv| !argv.is_empty()),
            "{template_field} should accompany {action_field} with a fillable argv_template: {parsed}"
        );
    }
}

fn assert_action_is_argv_or_template(label: &str, output: &Value, action: &str) {
    let concrete = !action.contains("...") && !action.contains('<');
    let template = &output["recommended_action_template"];
    // The canonical fillable template is always present for a valid action
    // and exposes a non-empty argv_template (HeddleCo/heddle#254).
    assert!(
        template["argv_template"]
            .as_array()
            .is_some_and(|argv| !argv.is_empty()),
        "{label} recommended action should expose a fillable template with argv_template: {output}"
    );
    let required_inputs = template["required_inputs"]
        .as_array()
        .unwrap_or_else(|| panic!("{label} template should list required_inputs: {output}"));
    if concrete {
        assert!(
            required_inputs.is_empty(),
            "{label} concrete recommended action template should need no inputs to run: {output}"
        );
    } else {
        assert!(
            !required_inputs.is_empty(),
            "{label} templated recommended action should require inputs before running: {output}"
        );
    }
}

fn assert_remote_divergence_surface(
    label: &str,
    output: &Value,
    expected_status: &str,
    expected_remote_drift: &str,
    expected_action: &str,
    expected_argv: Option<Value>,
) {
    let verification = if output.get("verification").is_some() {
        &output["verification"]
    } else {
        output
    };
    assert_eq!(
        verification["status"], expected_status,
        "{label} should report the same primary blocker: {output}"
    );
    assert_eq!(
        verification["remote_drift"], expected_remote_drift,
        "{label} should report the same remote drift: {output}"
    );
    assert_eq!(
        output["recommended_action"], expected_action,
        "{label} top-level action should match verification: {output}"
    );
    assert_eq!(
        verification["recommended_action"], expected_action,
        "{label} verification action should match top-level action: {output}"
    );
    if output.get("health").is_some() {
        assert_eq!(
            output["health"]["recommended_action"], expected_action,
            "{label} health action should match verification action: {output}"
        );
    }
    match expected_argv {
        Some(argv) => {
            assert_eq!(
                output["recommended_action_template"]["argv_template"], argv,
                "{label} should expose executable argv_template for the primary action: {output}"
            );
        }
        None => assert_action_is_argv_or_template(label, output, expected_action),
    }
}

#[test]
fn git_overlay_imported_ref_preview_diff_uses_merge_tree() {
    let temp = TempDir::new().unwrap();
    setup_diverged_imported_git_ref(temp.path());

    let parsed = json(
        temp.path(),
        &[
            "--output",
            "json",
            "merge",
            "feature",
            "--preview",
            "--with-diff",
        ],
    );

    assert_eq!(parsed["status"], "completed", "{parsed}");
    assert_eq!(parsed["semantic_result"], "clean_apply", "{parsed}");
    assert_eq!(parsed["changed_path_count"], 2, "{parsed}");
    assert_eq!(parsed["recommended_action"], Value::Null, "{parsed}");
    assert_eq!(parsed["next_action"], Value::Null, "{parsed}");
    let diff = &parsed["diff"];
    assert_eq!(diff["to_state"], "<merged-preview>", "{parsed}");
    let changes = diff["changes"].as_array().expect("diff changes array");
    let paths = changes
        .iter()
        .filter_map(|change| change["path"].as_str())
        .collect::<Vec<_>>();
    assert!(
        paths.contains(&"feature.txt") && paths.contains(&"shared.txt"),
        "preview diff should include incoming changes: {parsed}"
    );
    assert_eq!(
        parsed["changed_paths"],
        serde_json::json!(paths),
        "{parsed}"
    );
    assert!(
        !changes
            .iter()
            .any(|change| change["path"] == "main2.txt" && change["kind"] == "deleted"),
        "preview diff must not show deletion of destination-only files: {parsed}"
    );
}

#[test]
fn git_overlay_imported_ref_fast_forward_preview_has_no_ship_action() {
    let temp = TempDir::new().unwrap();
    setup_linear_imported_git_ref(temp.path());

    let parsed = json(
        temp.path(),
        &[
            "--output",
            "json",
            "merge",
            "feature",
            "--preview",
            "--with-diff",
        ],
    );

    assert_eq!(parsed["status"], "preview", "{parsed}");
    assert_eq!(parsed["semantic_result"], "fast_forward", "{parsed}");
    assert_eq!(parsed["recommended_action"], Value::Null, "{parsed}");
    assert_eq!(parsed["recommended_action_argv"], Value::Null, "{parsed}");
    assert_eq!(parsed["next_action"], Value::Null, "{parsed}");
    assert_eq!(
        parsed["changed_paths"],
        serde_json::json!(["feature.txt"]),
        "{parsed}"
    );
    assert_eq!(parsed["diff"]["changed_path_count"], 1, "{parsed}");
}

#[test]
fn git_overlay_imported_ref_ready_and_ship_fail_closed() {
    let temp = TempDir::new().unwrap();
    setup_diverged_imported_git_ref(temp.path());

    for args in [
        vec!["--output", "json", "ready", "--thread", "feature"],
        vec![
            "--output",
            "json",
            "land",
            "--thread",
            "feature",
            "--no-push",
        ],
    ] {
        let output = heddle_output(&args, Some(temp.path())).expect("command should run");
        assert!(
            !output.status.success(),
            "imported Git ref must not be treated as a managed thread: {args:?}"
        );
        assert!(
            output.stdout.is_empty(),
            "JSON refusal should be emitted on stderr only: {args:?}"
        );
        let stderr = std::str::from_utf8(&output.stderr).unwrap();
        let envelope: Value = serde_json::from_str(stderr)
            .unwrap_or_else(|err| panic!("expected JSON envelope: {err}: {stderr}"));
        assert_eq!(
            envelope["kind"], "imported_git_ref_not_managed_thread",
            "{envelope}"
        );
        assert_eq!(
            envelope["primary_command"], "heddle bridge git reconcile --ref feature --preview",
            "{envelope}"
        );
    }
}

fn assert_git_overlay_basics(parsed: &Value) {
    assert_eq!(parsed["repository_capability"], "git-overlay");
    assert_eq!(parsed["storage_model"], "git+heddle-sidecar");
}

fn assert_verify_check_rows(parsed: &Value) {
    let checks = parsed["checks"]
        .as_array()
        .unwrap_or_else(|| panic!("verify output should expose checks: {parsed}"));
    for row in [
        "Git",
        "Heddle",
        "Mapping",
        "Worktree",
        "Remote",
        "Operation",
        "Workflow",
        "Machine contract",
        "Clone",
    ] {
        assert!(
            checks.iter().any(|check| check["name"] == row),
            "verify output should include `{row}` row: {parsed}"
        );
    }
}

fn init_heddle_conflict_repo(path: &std::path::Path) {
    heddle(&["init"], Some(path)).unwrap();
    std::fs::write(path.join("conflict.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "Base"], Some(path)).unwrap();
    heddle(&["thread", "create", "feature"], Some(path)).unwrap();
    heddle(&["thread", "switch", "feature"], Some(path)).unwrap();
    std::fs::write(path.join("conflict.txt"), "feature version\n").unwrap();
    heddle(&["capture", "-m", "Feature change"], Some(path)).unwrap();
    heddle(&["thread", "switch", "main"], Some(path)).unwrap();
    std::fs::write(path.join("conflict.txt"), "main version\n").unwrap();
    heddle(&["capture", "-m", "Main change"], Some(path)).unwrap();
    heddle(&["thread", "switch", "feature"], Some(path)).unwrap();
}

fn start_conflicted_heddle_merge(path: &std::path::Path) -> String {
    let output = heddle_output(&["merge", "main"], Some(path))
        .expect("heddle merge should run and report conflict state");
    assert!(
        !output.status.success(),
        "conflicted mutating merge should exit nonzero after writing its report"
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        stdout.contains("Conflict") || path.join(".heddle/MERGE_STATE").exists(),
        "heddle merge should persist an in-progress merge state for continue: {stdout}"
    );
    stdout
}

#[test]
fn git_overlay_matrix_commit_prefers_heddle_principal_over_git_identity() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(&["config", "user.name", "Repo Local"], temp.path());
    git(&["config", "user.email", "local@example.com"], temp.path());

    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("local.txt"), "local\n").unwrap();
    let output = heddle_output_with_env(
        &["commit", "-m", "local identity commit"],
        Some(temp.path()),
        &[
            ("HEDDLE_PRINCIPAL_NAME", "Heddle Principal"),
            ("HEDDLE_PRINCIPAL_EMAIL", "principal@example.com"),
        ],
    )
    .expect("heddle commit should run");
    assert!(
        output.status.success(),
        "heddle commit should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let identity = git_stdout(temp.path(), &["log", "-1", "--format=%an <%ae>%n%cn <%ce>"]);
    assert_eq!(
        identity,
        "Heddle Principal <principal@example.com>\nHeddle Principal <principal@example.com>"
    );
}

#[test]
fn git_overlay_matrix_commit_uses_local_git_identity_for_state_and_checkpoint() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(&["config", "user.name", "Repo Local"], temp.path());
    git(&["config", "user.email", "local@example.com"], temp.path());

    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("local.txt"), "local\n").unwrap();
    let commit = json(
        temp.path(),
        &["--output", "json", "commit", "-m", "local identity commit"],
    );

    assert_eq!(commit["principal"]["name"], "Repo Local");
    assert_eq!(commit["principal"]["email"], "local@example.com");
    let identity = git_stdout(temp.path(), &["log", "-1", "--format=%an <%ae>%n%cn <%ce>"]);
    assert_eq!(
        identity,
        "Repo Local <local@example.com>\nRepo Local <local@example.com>"
    );
}

#[test]
fn git_overlay_matrix_commit_prefers_local_git_identity_over_user_config_principal() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(&["config", "user.name", "Repo Local"], temp.path());
    git(&["config", "user.email", "local@example.com"], temp.path());
    let user_config = temp.path().join(".heddle-user/config.toml");
    std::fs::create_dir_all(user_config.parent().unwrap()).unwrap();
    std::fs::write(
        &user_config,
        "[principal]\nname = \"User Config\"\nemail = \"user@example.com\"\n",
    )
    .unwrap();

    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("local.txt"), "local\n").unwrap();
    let commit = json(
        temp.path(),
        &["--output", "json", "commit", "-m", "local identity commit"],
    );

    assert_eq!(commit["principal"]["name"], "Repo Local");
    assert_eq!(commit["principal"]["email"], "local@example.com");
    let identity = git_stdout(temp.path(), &["log", "-1", "--format=%an <%ae>%n%cn <%ce>"]);
    assert_eq!(
        identity,
        "Repo Local <local@example.com>\nRepo Local <local@example.com>"
    );
}

#[test]
fn git_overlay_matrix_isolated_commit_uses_parent_git_identity_before_user_config_principal() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(&["config", "user.name", "Audit User"], temp.path());
    git(&["config", "user.email", "audit@example.com"], temp.path());
    std::fs::write(temp.path().join("README.md"), "base\n").unwrap();
    git_commit_all(temp.path(), "base");
    heddle_adopt(temp.path());

    let user_config = temp.path().with_extension("audit-user-config.toml");
    std::fs::write(
        &user_config,
        "[principal]\nname = \"Audit Remote\"\nemail = \"remote@example.invalid\"\n",
    )
    .unwrap();

    let checkout = temp.path().with_extension("isolated-audit");
    json(
        temp.path(),
        &[
            "--output",
            "json",
            "start",
            "feature/audit",
            "--path",
            checkout.to_str().unwrap(),
        ],
    );

    std::fs::write(checkout.join("audit.txt"), "isolated\n").unwrap();
    let output = heddle_output_with_env(
        &["--output", "json", "commit", "-m", "isolated audit"],
        Some(&checkout),
        &[("HEDDLE_CONFIG", user_config.to_str().unwrap())],
    )
    .expect("isolated commit should run");
    assert!(
        output.status.success(),
        "isolated commit should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let commit: Value =
        serde_json::from_slice(&output.stdout).expect("isolated commit JSON should parse");
    assert_eq!(commit["principal"]["name"], "Audit User");
    assert_eq!(commit["principal"]["email"], "audit@example.com");
}

#[test]
fn git_overlay_matrix_commit_prefers_repo_principal_over_git_identity() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(&["config", "user.name", "Repo Local"], temp.path());
    git(&["config", "user.email", "local@example.com"], temp.path());

    heddle(&["init"], Some(temp.path())).unwrap();
    let config_path = temp.path().join(".heddle/config.toml");
    let mut config = std::fs::read_to_string(&config_path).unwrap();
    config.push_str("\n[principal]\nname = \"Repo Principal\"\nemail = \"repo@example.com\"\n");
    std::fs::write(&config_path, config).unwrap();

    std::fs::write(temp.path().join("repo.txt"), "repo\n").unwrap();
    let commit = json(
        temp.path(),
        &["--output", "json", "commit", "-m", "repo identity commit"],
    );

    assert_eq!(commit["principal"]["name"], "Repo Principal");
    assert_eq!(commit["principal"]["email"], "repo@example.com");
    let identity = git_stdout(temp.path(), &["log", "-1", "--format=%an <%ae>%n%cn <%ce>"]);
    assert_eq!(
        identity,
        "Repo Principal <repo@example.com>\nRepo Principal <repo@example.com>"
    );
}

#[test]
fn git_overlay_matrix_commit_uses_global_git_identity_when_repo_local_absent() {
    let temp = TempDir::new().unwrap();
    let global_home = TempDir::new().unwrap();
    let global_config = temp.path().join("global.gitconfig");
    std::fs::write(
        &global_config,
        "[user]\n\tname = Global User\n\temail = global@example.com\n",
    )
    .unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(&["config", "--unset", "user.name"], temp.path());
    git(&["config", "--unset", "user.email"], temp.path());

    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("global.txt"), "global\n").unwrap();
    let output = heddle_output_with_env(
        &["commit", "-m", "global identity commit"],
        Some(temp.path()),
        &[
            ("GIT_CONFIG_GLOBAL", global_config.to_str().unwrap()),
            ("HOME", global_home.path().to_str().unwrap()),
            ("XDG_CONFIG_HOME", global_home.path().to_str().unwrap()),
        ],
    )
    .expect("heddle commit should run");
    assert!(
        output.status.success(),
        "heddle commit should accept Git's global identity fallback: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let identity = git_stdout(temp.path(), &["log", "-1", "--format=%an <%ae>%n%cn <%ce>"]);
    assert_eq!(
        identity,
        "Global User <global@example.com>\nGlobal User <global@example.com>"
    );
}

#[test]
fn git_overlay_matrix_checkpoint_message_controls_git_subject() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");

    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("captured.txt"), "captured\n").unwrap();
    heddle(
        &["capture", "-m", "captured heddle intent"],
        Some(temp.path()),
    )
    .unwrap();
    heddle(
        &["checkpoint", "-m", "checkpoint git subject"],
        Some(temp.path()),
    )
    .unwrap();

    let subject = git_stdout(temp.path(), &["log", "-1", "--format=%s"]);
    assert_eq!(subject, "checkpoint git subject");
}

#[test]
fn git_overlay_matrix_commit_without_git_identity_uses_heddle_principal() {
    let temp = TempDir::new().unwrap();
    let global_home = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(&["config", "--unset", "user.name"], temp.path());
    git(&["config", "--unset", "user.email"], temp.path());

    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("principal.txt"), "principal\n").unwrap();
    let output = heddle_output_with_env(
        &["commit", "-m", "heddle principal commit"],
        Some(temp.path()),
        &[
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("HOME", global_home.path().to_str().unwrap()),
            ("XDG_CONFIG_HOME", global_home.path().to_str().unwrap()),
            ("HEDDLE_PRINCIPAL_NAME", "Heddle Principal"),
            ("HEDDLE_PRINCIPAL_EMAIL", "principal@example.com"),
        ],
    )
    .expect("heddle commit should run");
    assert!(
        output.status.success(),
        "commit should use Heddle principal without Git config: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let identity = git_stdout(temp.path(), &["log", "-1", "--format=%an <%ae>%n%cn <%ce>"]);
    assert_eq!(
        identity,
        "Heddle Principal <principal@example.com>\nHeddle Principal <principal@example.com>"
    );
}

#[test]
fn git_overlay_matrix_commit_without_any_identity_refuses_before_capture() {
    let temp = TempDir::new().unwrap();
    let global_home = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    git_commit_all(temp.path(), "seed");
    git(&["config", "--unset", "user.name"], temp.path());
    git(&["config", "--unset", "user.email"], temp.path());

    let before_head = git_stdout(temp.path(), &["rev-parse", "HEAD"]);
    heddle_adopt(temp.path());
    let before_state = json(temp.path(), &["status", "--output", "json"])["current_state"]
        .as_str()
        .expect("adopted repo should have a current state")
        .to_string();

    std::fs::write(temp.path().join("no-identity.txt"), "anonymous?\n").unwrap();
    let output = heddle_output_with_env(
        &[
            "--output",
            "json",
            "commit",
            "-m",
            "should not become unknown",
        ],
        Some(temp.path()),
        &[
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("HOME", global_home.path().to_str().unwrap()),
            ("XDG_CONFIG_HOME", global_home.path().to_str().unwrap()),
            ("HEDDLE_PRINCIPAL_NAME", ""),
            ("HEDDLE_PRINCIPAL_EMAIL", ""),
        ],
    )
    .expect("heddle commit should run");
    assert!(
        !output.status.success(),
        "commit should refuse missing identity"
    );
    assert!(output.stdout.is_empty());
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("missing identity should emit JSON envelope");
    assert_eq!(envelope["kind"], "git_checkpoint_identity_required");
    assert!(
        envelope["unsafe_condition"]
            .as_str()
            .is_some_and(|condition| condition.contains("Unknown <unknown@example.com>")),
        "identity refusal should name the unsafe fallback: {stderr}"
    );
    assert_eq!(git_stdout(temp.path(), &["rev-parse", "HEAD"]), before_head);
    assert_eq!(
        json(temp.path(), &["status", "--output", "json"])["current_state"],
        before_state,
        "missing identity refusal must happen before preserving a Heddle capture"
    );
}

#[test]
fn git_overlay_matrix_commit_no_all_nothing_staged_refuses_before_identity_preflight() {
    // `--no-all` with the index identical to HEAD has nothing to commit. That
    // nothing-to-commit short-circuit must run BEFORE the identity / ref-update
    // preflights: a repo with no configured identity must still get the
    // nothing-to-commit outcome, not an identity-required refusal on a commit
    // that was never going to write anything.
    let temp = TempDir::new().unwrap();
    let global_home = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    git_commit_all(temp.path(), "seed");
    git(&["config", "--unset", "user.name"], temp.path());
    git(&["config", "--unset", "user.email"], temp.path());

    let before_head = git_stdout(temp.path(), &["rev-parse", "HEAD"]);
    heddle_adopt(temp.path());

    // Nothing genuinely staged (index == HEAD); only worktree edits + an
    // untracked file make the worktree dirty.
    std::fs::write(temp.path().join("base.txt"), "base\nworktree edit\n").unwrap();
    std::fs::write(temp.path().join("scratch.txt"), "untracked\n").unwrap();

    let output = heddle_output_with_env(
        &["--output", "json", "commit", "--no-all", "-m", "index only"],
        Some(temp.path()),
        &[
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("HOME", global_home.path().to_str().unwrap()),
            ("XDG_CONFIG_HOME", global_home.path().to_str().unwrap()),
            ("HEDDLE_PRINCIPAL_NAME", ""),
            ("HEDDLE_PRINCIPAL_EMAIL", ""),
        ],
    )
    .expect("heddle commit --no-all should run");
    assert!(
        !output.status.success(),
        "commit --no-all with nothing staged must refuse: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("refusal should emit JSON envelope");
    assert_eq!(
        envelope["kind"], "nothing_to_commit",
        "--no-all with nothing staged must surface nothing-to-commit before the identity preflight, not an identity refusal: {stderr}"
    );
    assert_eq!(
        git_stdout(temp.path(), &["rev-parse", "HEAD"]),
        before_head,
        "--no-all must not create a commit when nothing is staged"
    );
}

#[test]
fn git_overlay_matrix_plain_git_no_commit_bootstrap_commands() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "trunk");

    std::fs::write(temp.path().join("pending.txt"), "pending").unwrap();

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["repository_capability"], "plain-git");
    assert_eq!(status["heddle_initialized"], false);
    assert_eq!(status["git_branch"], "trunk");
    assert_eq!(status["git_overlay_health"]["status"], "needs_init");
    assert_eq!(status["recommended_action"], "heddle init");
    assert_eq!(status["verification"]["recommended_action"], "heddle init");
    assert!(
        status["verification"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check["name"] == "Mapping" && check["status"] == "no_commits"),
        "unborn Git repos should not recommend importing a non-existent branch: {status}"
    );
    assert!(
        !temp.path().join(".heddle").exists(),
        "status in a plain Git repo must be probe-only"
    );
    let status_text = heddle(&["status", "--output", "text"], Some(temp.path())).unwrap();
    assert!(
        status_text.contains("initialize Heddle with heddle init")
            && status_text.contains("no Git commits to import yet")
            && !status_text.contains("connect this branch with heddle adopt"),
        "unborn status text should describe initialization, not adoption: {status_text}"
    );
    let verify_text = heddle_output(&["verify", "--output", "text"], Some(temp.path()))
        .expect("verify should run");
    assert!(
        !verify_text.status.success(),
        "unborn verify should fail until init"
    );
    let verify_text = String::from_utf8_lossy(&verify_text.stdout);
    assert!(
        verify_text.contains("initialize Heddle with heddle init")
            && verify_text.contains("no Git commits to import yet")
            && !verify_text.contains("connect this branch with heddle adopt"),
        "unborn verify text should describe initialization, not adoption: {verify_text}"
    );
    let bridge = json(
        temp.path(),
        &["bridge", "git", "status", "--output", "json"],
    );
    assert_eq!(bridge["recommended_action"], "heddle init");
    assert_eq!(bridge["verification"]["recommended_action"], "heddle init");
    assert_eq!(bridge["verification"]["import_state"], "no_commits");
    assert_eq!(bridge["verification"]["mapping_state"], "no_commits");
    assert!(bridge["git_overlay_import_hint"].is_null());
    let bridge_text = heddle(
        &["bridge", "git", "status", "--output", "text"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        bridge_text.contains("run `heddle init`")
            && bridge_text.contains("Git import: no commits to import yet")
            && !bridge_text.contains("run `heddle adopt`"),
        "unborn bridge status text should not recommend invalid adoption: {bridge_text}"
    );
    let diagnose = json(temp.path(), &["doctor", "--output", "json"]);
    assert_eq!(diagnose["recommended_action"], "heddle init");
    assert_eq!(diagnose["git_overlay_import_hint"], Value::Null);

    let failed_adopt = heddle_output(
        &["--output", "json", "adopt", "--ref", "trunk"],
        Some(temp.path()),
    )
    .expect("adopt should run");
    assert!(
        !failed_adopt.status.success(),
        "adopt should fail before side effects when Git has no commits"
    );
    let stderr = String::from_utf8_lossy(&failed_adopt.stderr);
    assert!(
        stderr.contains("git_history_empty") && stderr.contains("heddle init"),
        "unborn adopt refusal should point at init: {stderr}"
    );
    assert!(
        !temp.path().join(".heddle").exists(),
        "failed unborn adopt must not leave partial Heddle metadata"
    );

    heddle(&["init"], Some(temp.path())).unwrap();

    let diagnose = json(temp.path(), &["doctor", "--output", "json"]);
    assert_git_overlay_basics(&diagnose);
    assert_eq!(diagnose["thread"]["name"], "trunk");

    let thread_list = json(temp.path(), &["thread", "list", "--output", "json"]);
    assert_eq!(thread_list["current"], "trunk");

    let workspace = json(temp.path(), &["workspace", "show", "--output", "json"]);
    assert_eq!(workspace["current_thread"], "trunk");

    let show = json(temp.path(), &["show", "HEAD", "--output", "json"]);
    assert_git_overlay_basics(&show);
    assert!(show["change_id"].as_str().is_some());

    let log = json(temp.path(), &["log", "--output", "json"]);
    assert_git_overlay_basics(&log);
    assert!(
        !log["states"].as_array().unwrap().is_empty(),
        "log should bootstrap a visible state in plain Git no-commit repos: {log}"
    );
}

#[test]
fn git_overlay_matrix_plain_git_with_branches_and_tags_recommends_adopt_all() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");
    git(&["branch", "feature"], temp.path());
    git(&["tag", "v0.1.0"], temp.path());

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["repository_capability"], "plain-git");
    assert_eq!(status["recommended_action"], "heddle adopt");
    assert_eq!(
        status["recommended_action_template"]["argv_template"],
        heddle_argv_json(["adopt"])
    );
    assert_eq!(status["verification"]["recommended_action"], "heddle adopt");
    assert!(
        status["verification"]["checks"][0]["details"]["git_branch_count"] == "2"
            && status["verification"]["checks"][0]["details"]["git_tag_count"] == "1",
        "plain Git probe should explain why all-ref adoption is recommended: {status}"
    );

    heddle(&["adopt"], Some(temp.path())).unwrap();
    let verify = json(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(verify["status"], "clean");
    assert_eq!(verify["recommended_action"], Value::Null);
}

#[test]
fn git_overlay_matrix_verify_tracks_plain_init_import_clean_loop() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");

    let verify = json(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(verify["verified"], false);
    assert_eq!(verify["status"], "needs_init");
    assert_eq!(verify["recommended_action"], "heddle adopt --ref main");
    assert_verify_check_rows(&verify);
    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["verification"]["verified"], false);
    assert_eq!(status["verification"]["status"], "needs_init");
    assert_eq!(status["recommended_action"], "heddle adopt --ref main");
    assert_eq!(status["recovery_commands"][0], "heddle adopt --ref main");
    assert_eq!(status["recovery_commands"][1], "heddle adopt");
    assert_eq!(status["recovery_commands"][2], "heddle init");
    assert_verify_check_rows(&status["verification"]);
    assert!(
        !temp.path().join(".heddle").exists(),
        "verify in a plain Git repo must be observe-only"
    );
    let status_text = heddle(&["status", "--output", "text"], Some(temp.path())).unwrap();
    assert_eq!(
        status_text.matches("Setup needed:").count(),
        1,
        "plain Git status should print one setup line, not duplicate import/setup advice: {status_text}"
    );
    assert!(
        status_text.contains("heddle adopt --ref main"),
        "plain Git status should still name the exact adoption command: {status_text}"
    );

    let bridge = json(
        temp.path(),
        &["bridge", "git", "status", "--output", "json"],
    );
    assert_eq!(bridge["git_overlay_health"]["status"], "needs_init");
    assert_eq!(bridge["verification"]["status"], "needs_init");
    assert_eq!(bridge["verification"]["import_state"], "needs_import");
    assert_eq!(bridge["verification"]["mapping_state"], "needs_import");
    assert_eq!(
        bridge["git_overlay_import_hint"]["recommended_command"],
        "heddle adopt --ref main"
    );
    assert!(
        bridge["git_overlay_import_hint"]["missing_branches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|branch| branch.as_str() == Some("main")),
        "bridge status should not call the active unimported branch in sync before setup: {bridge}"
    );
    assert_verify_check_rows(&bridge["verification"]);
    assert!(
        !temp.path().join(".heddle").exists(),
        "bridge git status in a plain Git repo must be observe-only"
    );

    heddle(&["init"], Some(temp.path())).unwrap();
    let verify = json(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(verify["verified"], false);
    assert_eq!(verify["status"], "needs_import");
    assert_verify_check_rows(&verify);
    assert_eq!(verify["recommended_action"], "heddle adopt --ref main");
    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["verification"]["verified"], false);
    assert_eq!(status["verification"]["status"], "needs_import");
    assert_eq!(status["recommended_action"], "heddle adopt --ref main");
    assert_eq!(status["recovery_commands"][0], "heddle adopt --ref main");
    assert_verify_check_rows(&status["verification"]);
    let status_text = heddle(&["status", "--output", "text"], Some(temp.path())).unwrap();
    assert_eq!(
        status_text.matches("Setup needed:").count(),
        1,
        "initialized-but-unimported status should print one setup line, not duplicate import/setup advice: {status_text}"
    );
    assert!(
        status_text.contains("heddle adopt --ref main"),
        "initialized-but-unimported status should still name the exact adoption command: {status_text}"
    );
    let workspace = json(temp.path(), &["workspace", "show", "--output", "json"]);
    assert_eq!(workspace["verification"]["verified"], false);
    assert_eq!(workspace["verification"]["status"], "needs_import");
    assert_eq!(workspace["recommended_action"], "heddle adopt --ref main");
    assert!(
        workspace["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|group| group["threads"].as_array().unwrap().iter())
            .all(|thread| thread["recommended_action"] == "heddle adopt --ref main"),
        "workspace should keep import repair actions while repository verification is blocked: {workspace}"
    );
    assert_verify_check_rows(&workspace["verification"]);
    let thread_list = json(temp.path(), &["thread", "list", "--output", "json"]);
    assert_eq!(thread_list["verification"]["verified"], false);
    assert_eq!(thread_list["verification"]["status"], "needs_import");
    assert_eq!(thread_list["recommended_action"], "heddle adopt --ref main");
    assert!(
        thread_list["threads"]
            .as_array()
            .unwrap()
            .iter()
            .all(|thread| thread["recommended_action"] == "heddle adopt --ref main"),
        "thread list should keep import repair actions while repository verification is blocked: {thread_list}"
    );
    assert_verify_check_rows(&thread_list["verification"]);
    let thread_show = json(temp.path(), &["thread", "show", "main", "--output", "json"]);
    assert_eq!(thread_show["verification"]["verified"], false);
    assert_eq!(thread_show["verification"]["status"], "needs_import");
    assert_eq!(thread_show["recommended_action"], "heddle adopt --ref main");
    assert_verify_check_rows(&thread_show["verification"]);
    let diagnose = json(temp.path(), &["doctor", "--output", "json"]);
    assert_eq!(diagnose["verification"]["verified"], false);
    assert_eq!(diagnose["verification"]["status"], "needs_import");
    assert_eq!(
        diagnose["verification"]["recommended_action"],
        "heddle adopt --ref main"
    );
    assert_eq!(diagnose["recommended_action"], "heddle adopt --ref main");
    assert_eq!(diagnose["recovery_commands"][0], "heddle adopt --ref main");
    let bridge = json(
        temp.path(),
        &["bridge", "git", "status", "--output", "json"],
    );
    assert_eq!(bridge["verification"]["status"], "needs_import");
    assert_eq!(bridge["recommended_action"], "heddle adopt --ref main");
    assert_eq!(bridge["recovery_commands"][0], "heddle adopt --ref main");
    let bridge_text = heddle(
        &["bridge", "git", "status", "--output", "text"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        !bridge_text.contains("heddle bridge git init"),
        "initialized-but-unimported bridge status should not recommend stale bridge init ceremony: {bridge_text}"
    );
    assert!(
        bridge_text.contains("the import step will create it")
            && bridge_text.contains("heddle adopt --ref main"),
        "bridge status text should point only at import while needs_import: {bridge_text}"
    );

    heddle(&["adopt", "--ref", "main"], Some(temp.path())).unwrap();
    let verify = json(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(verify["verified"], true);
    assert_eq!(verify["status"], "clean");
    assert_verify_check_rows(&verify);
    let verify_text = heddle(&["verify", "--output", "text"], Some(temp.path())).unwrap();
    assert!(
        !verify_text.contains("not checked"),
        "clean verify text should render all proof rows as checked: {verify_text}"
    );
    let diagnose = json(temp.path(), &["doctor", "--output", "json"]);
    assert_eq!(diagnose["output_kind"], "diagnose");
    assert_eq!(diagnose["verification"]["verified"], true);
    assert_eq!(diagnose["verification"]["status"], "clean");
    assert_eq!(diagnose["recommended_action"], Value::Null);
    assert_eq!(diagnose["health"]["recommended_action"], Value::Null);
    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["output_kind"], "status");
    assert_eq!(status["verification"]["verified"], true);
    assert_eq!(status["verification"]["status"], "clean");
    assert_eq!(status["recommended_action"], Value::Null);
    assert_eq!(
        status["recovery_commands"]
            .as_array()
            .expect("status recovery commands should be an array")
            .len(),
        0
    );
    assert_verify_check_rows(&status["verification"]);
    let workspace = json(temp.path(), &["workspace", "show", "--output", "json"]);
    assert_eq!(workspace["output_kind"], "workspace_summary");
    assert_eq!(workspace["verification"]["verified"], true);
    assert_eq!(workspace["verification"]["status"], "clean");
    assert_eq!(workspace["recommended_action"], Value::Null);
    assert_verify_check_rows(&workspace["verification"]);
    let thread_list = json(temp.path(), &["thread", "list", "--output", "json"]);
    assert_eq!(thread_list["output_kind"], "thread_list");
    assert_eq!(thread_list["verification"]["verified"], true);
    assert_eq!(thread_list["verification"]["status"], "clean");
    assert_eq!(thread_list["recommended_action"], Value::Null);
    assert_verify_check_rows(&thread_list["verification"]);
    let bridge = json(
        temp.path(),
        &["bridge", "git", "status", "--output", "json"],
    );
    assert_eq!(bridge["output_kind"], "bridge_git_status");
    assert_eq!(bridge["verification"]["verified"], true);
    assert_eq!(bridge["verification"]["status"], "clean");
    assert_eq!(bridge["recommended_action"], Value::Null);
    let thread_show = json(temp.path(), &["thread", "show", "main", "--output", "json"]);
    assert_eq!(thread_show["verification"]["verified"], true);
    assert_eq!(thread_show["verification"]["status"], "clean");
    assert_verify_check_rows(&thread_show["verification"]);
    let ready = json(
        temp.path(),
        &["--output", "json", "ready", "-m", "first-run ready state"],
    );
    assert_eq!(ready["verification"]["verified"], true);
    assert_eq!(ready["verification"]["status"], "clean");
    assert_verify_check_rows(&ready["verification"]);
}

#[test]
fn git_overlay_matrix_adopt_initializes_imports_and_verifies() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");
    git(&["branch", "support/import-me"], temp.path());
    git(&["tag", "v1.0.0"], temp.path());

    let before = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(before["recommended_action"], "heddle adopt");
    assert!(
        !temp.path().join(".heddle").exists(),
        "status before adopt must be observe-only"
    );

    let adopted = json(temp.path(), &["adopt", "--output", "json"]);
    assert_eq!(adopted["output_kind"], "adopt");
    assert_eq!(adopted["adopted"], true);
    assert_eq!(adopted["initialized"], true);
    assert_eq!(adopted["branches_synced"], 2);
    assert_eq!(adopted["tags_synced"], 1);
    assert_eq!(adopted["verification"]["verified"], true);
    assert_eq!(adopted["verification"]["status"], "clean");
    assert_eq!(git_status_short(temp.path()), "");

    let verify = json(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(verify["verified"], true);
    assert_eq!(verify["status"], "clean");
    assert_verify_check_rows(&verify);
    let bridge = json(
        temp.path(),
        &["bridge", "git", "status", "--output", "json"],
    );
    assert_eq!(bridge["verification"]["verified"], true);
    assert_eq!(bridge["git_overlay_import_hint"], Value::Null);
}

#[test]
fn git_overlay_matrix_verify_reports_git_tags_created_after_adoption() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle(&["adopt"], Some(temp.path())).unwrap();

    git(&["tag", "v2.0.0"], temp.path());

    let verify = json(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(verify["verified"], false);
    assert_eq!(verify["status"], "tags_need_import");
    assert_eq!(verify["mapping_state"], "tags_need_import");
    assert_eq!(verify["import_state"], "tags_need_import");
    assert_eq!(verify["recommended_action"], "heddle adopt --ref v2.0.0");
    assert_eq!(
        verify["recommended_action_template"]["argv_template"],
        heddle_argv_json(["adopt", "--ref", "v2.0.0"])
    );
    assert!(
        verify["checks"].as_array().unwrap().iter().any(|check| {
            check["name"] == "Mapping"
                && check["status"] == "tags_need_import"
                && check["summary"]
                    .as_str()
                    .is_some_and(|summary| summary.contains("v2.0.0"))
        }),
        "verify should surface missing Git tag markers through the public Mapping row: {verify}"
    );

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["verification"]["status"], "tags_need_import");
    assert_eq!(status["recommended_action"], "heddle adopt --ref v2.0.0");

    let adopted = json(
        temp.path(),
        &["adopt", "--ref", "v2.0.0", "--output", "json"],
    );
    assert_eq!(adopted["tags_synced"], 1);
    assert_eq!(adopted["verification"]["verified"], true);
    assert_eq!(adopted["verification"]["status"], "clean");
}

#[test]
fn git_overlay_matrix_verify_reports_moved_git_tag_before_adoption() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "one\n").unwrap();
    git_commit_all(temp.path(), "one");
    git(&["tag", "v1.0.0"], temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "two\n").unwrap();
    git_commit_all(temp.path(), "two");
    heddle(&["adopt"], Some(temp.path())).unwrap();

    git(&["tag", "-f", "v1.0.0", "HEAD"], temp.path());

    let verify = json(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(verify["verified"], false);
    assert_eq!(verify["status"], "tag_marker_mismatch");
    assert_eq!(verify["mapping_state"], "tag_marker_mismatch");
    assert_eq!(verify["recommended_action"], "heddle adopt --ref v1.0.0");
    assert!(
        verify["checks"].as_array().unwrap().iter().any(|check| {
            check["name"] == "Mapping"
                && check["status"] == "tag_marker_mismatch"
                && check["details"]["mismatched_tags"]
                    .as_str()
                    .is_some_and(|details| details.contains("v1.0.0"))
        }),
        "verify should report moved Git tag marker mismatches: {verify}"
    );

    let adopted = json(
        temp.path(),
        &["adopt", "--ref", "v1.0.0", "--output", "json"],
    );
    assert_eq!(adopted["tags_synced"], 1);
    assert_eq!(adopted["verification"]["verified"], true);
    assert_eq!(adopted["verification"]["status"], "clean");
}

#[test]
fn git_overlay_matrix_selective_branch_adopt_surfaces_remaining_tag_import() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");
    git(&["tag", "v1.0.0"], temp.path());

    let adopted = json(temp.path(), &["adopt", "--ref", "main", "--output", "json"]);
    assert_eq!(adopted["tags_synced"], 0);
    assert_eq!(adopted["verification"]["verified"], false);
    assert_eq!(adopted["verification"]["status"], "tags_need_import");
    assert_eq!(adopted["recommended_action"], "heddle adopt --ref v1.0.0");
}

#[test]
fn git_overlay_matrix_new_branch_at_adopted_tip_verifies_without_setup_loop() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    git_commit_all(temp.path(), "seed");

    heddle(&["adopt", "--ref", "main"], Some(temp.path())).unwrap();
    let adopted = json(temp.path(), &["show", "HEAD", "--output", "json"]);
    let adopted_change = adopted["change_id"]
        .as_str()
        .expect("adopted state should have short change id")
        .to_string();

    git(&["checkout", "-b", "scratch"], temp.path());

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["thread"], "scratch");
    assert_eq!(status["verification"]["verified"], true);
    assert_eq!(status["verification"]["status"], "clean");
    assert_eq!(status["verification"]["mapping_state"], "clean");
    assert_eq!(status["verification"]["import_state"], "clean");
    assert_eq!(
        status["recommended_action"],
        Value::Null,
        "a new Git branch at an already-adopted commit should not look like setup work: {status}"
    );
    assert!(status["state"]["change_id"].as_str().is_some());
    assert!(status["current_state"].as_str().is_some());

    let status_text = heddle(&["status", "--output", "text"], Some(temp.path())).unwrap();
    assert!(
        status_text.contains("Heddle status for scratch")
            && status_text.contains("Checkout: Git branch checkout")
            && !status_text.contains("Setup needed")
            && !status_text.contains("main checkout")
            && !status_text.contains("heddle adopt --ref scratch"),
        "status text should agree with the checked-out Git branch without repeating setup copy: {status_text}"
    );

    let bridge = json(
        temp.path(),
        &["bridge", "git", "status", "--output", "json"],
    );
    assert_eq!(bridge["verification"]["verified"], true);
    assert_eq!(bridge["verification"]["status"], "clean");
    assert_eq!(bridge["git_overlay_import_hint"], Value::Null);

    std::fs::write(temp.path().join("scratch.txt"), "scratch\n").unwrap();
    heddle(&["capture", "-m", "scratch work"], Some(temp.path())).unwrap();
    let captured = json(temp.path(), &["show", "HEAD", "--output", "json"]);
    assert!(
        captured["parents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|parent| parent.as_str() == Some(adopted_change.as_str())),
        "capture on the recognized branch should preserve the adopted state as its parent, not create a root: {captured}"
    );
}

#[test]
fn git_overlay_matrix_commit_after_adopt_ref_checkpoints_without_import_loop() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    git_commit_all(temp.path(), "seed");

    heddle(&["adopt", "--ref", "main"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("base.txt"), "changed\n").unwrap();

    let commit = json(temp.path(), &["--output", "json", "commit", "-m", "change"]);
    assert_eq!(commit["output_kind"], "commit");
    assert!(commit["change_id"].as_str().is_some());
    assert!(commit["git_commit"].as_str().is_some());
    assert_eq!(commit["verification"]["verified"], true);
    assert_eq!(commit["verification"]["status"], "clean");
    assert_eq!(
        commit["verification"]["recommended_action"],
        Value::Null,
        "commit after single-ref adoption should checkpoint instead of falling into needs_import: {commit}"
    );
    assert_eq!(git_status_short(temp.path()), "");

    let verify = json(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(verify["verified"], true);
    assert_eq!(verify["status"], "clean");
    assert_eq!(verify["recommended_action"], Value::Null);
}

#[test]
fn git_overlay_matrix_ready_blocks_when_repository_verification_needs_import() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle(&["init"], Some(temp.path())).unwrap();

    let ready = json(temp.path(), &["--output", "json", "ready"]);
    assert_eq!(ready["status"], "blocked");
    assert_eq!(ready["verification"]["verified"], false);
    assert_eq!(ready["verification"]["status"], "needs_import");
    assert_eq!(ready["recommended_action"], "heddle adopt --ref main");
    assert!(
        ready["message"]
            .as_str()
            .is_some_and(|message| message.contains("repository verification is restored")),
        "ready should name the verify blocker instead of claiming ready: {ready}"
    );
    assert_verify_check_rows(&ready["verification"]);
}

#[test]
fn git_overlay_matrix_ready_thread_keeps_verification_clean_and_workflow_actionable() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle_adopt(temp.path());

    let started = json(
        temp.path(),
        &[
            "--output",
            "json",
            "start",
            "feature/ready-verify",
            "--workspace",
            "materialized",
        ],
    );
    let thread_path = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());
    std::fs::write(thread_path.join("feature.txt"), "ready work\n").unwrap();

    let ready = json(
        &thread_path,
        &["--output", "json", "ready", "-m", "ready thread work"],
    );
    assert_eq!(ready["status"], "completed");
    let parent_land_action = format!(
        "heddle --repo {} land --thread feature/ready-verify --no-push",
        temp.path().display()
    );
    assert_eq!(
        ready["recommended_action"], parent_land_action,
        "ready from an isolated checkout should print a command that works from that checkout: {ready}"
    );
    assert_eq!(ready["verification"]["verified"], true);
    assert_eq!(ready["verification"]["status"], "clean");
    assert_eq!(ready["verification"]["workflow_status"], "ready");
    assert!(
        ready["verification"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check["name"] == "Workflow"
                && check["status"] == "ready"
                && check["clean"] == true),
        "workflow attention should stay actionable without making repository verification false: {ready}"
    );
    // The mutation reply no longer carries verification on the wire,
    // so we rely on the top-level recommended_action to capture the
    // context-aware command (asserted above). The injected verification
    // proof (grafted from a separate verify call) carries a plain
    // recommendation without the original `--repo` prefix, which is
    // expected: the separate verify call has no caller context.
    let _ = parent_land_action;
    assert_eq!(
        ready["verification"]["recovery_commands"],
        serde_json::json!([])
    );
    assert_verify_check_rows(&ready["verification"]);

    let parent_status_before_preview = json(temp.path(), &["--output", "json", "status"]);
    assert_eq!(
        parent_status_before_preview["recommended_action"],
        "heddle land --thread feature/ready-verify --no-push",
        "parent status should keep ready workflow actionable: {parent_status_before_preview}"
    );
    let thread_list_before_preview = json(temp.path(), &["--output", "json", "thread", "list"]);
    assert_eq!(
        thread_list_before_preview["recommended_action"],
        "heddle land --thread feature/ready-verify --no-push",
        "thread list top-level action should match verify after ready: {thread_list_before_preview}"
    );
    assert_eq!(
        thread_list_before_preview["recommended_action_template"]["argv_template"],
        heddle_argv_json(["land", "--thread", "feature/ready-verify", "--no-push"]),
        "thread list top-level action should be directly executable: {thread_list_before_preview}"
    );
    let workspace_before_preview = json(temp.path(), &["--output", "json", "workspace", "show"]);
    assert_eq!(
        workspace_before_preview["recommended_action"],
        "heddle land --thread feature/ready-verify --no-push",
        "workspace top-level action should match verify after ready: {workspace_before_preview}"
    );
    assert_eq!(
        workspace_before_preview["recommended_action_template"]["argv_template"],
        heddle_argv_json(["land", "--thread", "feature/ready-verify", "--no-push"]),
        "workspace top-level action should be directly executable: {workspace_before_preview}"
    );

    let status_text = heddle(&["status", "--output", "text"], Some(&thread_path)).unwrap();
    assert!(
        status_text.contains("Thread changes vs target: 1")
            && status_text.contains("No unsaved changes, worktree clean"),
        "ready thread status should distinguish captured thread changes from unsaved worktree edits: {status_text}"
    );
    let status_json = json(&thread_path, &["--output", "json", "status"]);
    assert_eq!(
        status_json["changed_path_count"], 1,
        "aggregate status count should still include captured thread delta: {status_json}"
    );
    assert_eq!(
        status_json["worktree_changed_path_count"], 0,
        "ready thread status should expose clean worktree count separately: {status_json}"
    );
    assert_eq!(
        status_json["thread_changed_path_count"], 1,
        "ready thread status should expose captured thread delta separately: {status_json}"
    );

    let thread_show_before_preview = json(
        temp.path(),
        &["--output", "json", "thread", "show", "feature/ready-verify"],
    );
    assert_eq!(
        thread_show_before_preview["recommended_action"],
        "heddle land --thread feature/ready-verify --no-push",
        "thread show should follow the canonical land path after ready: {thread_show_before_preview}"
    );
    let preview = json(
        temp.path(),
        &[
            "--output",
            "json",
            "merge",
            "feature/ready-verify",
            "--preview",
        ],
    );
    assert_eq!(
        preview["recommended_action"],
        "heddle land --thread feature/ready-verify --no-push"
    );
    let thread_show_after_preview = json(
        temp.path(),
        &["--output", "json", "thread", "show", "feature/ready-verify"],
    );
    assert_eq!(
        thread_show_after_preview["recommended_action"],
        "heddle land --thread feature/ready-verify --no-push",
        "thread show should keep the established land path after merge preview: {thread_show_after_preview}"
    );
    assert_eq!(
        thread_show_after_preview["verification"]["recommended_action"],
        "heddle land --thread feature/ready-verify --no-push",
        "nested verify should not send agents back to preview after preview already succeeded: {thread_show_after_preview}"
    );
    assert!(
        thread_show_after_preview["verification"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check["name"] == "Workflow"
                && check["recommended_action"]
                    == "heddle land --thread feature/ready-verify --no-push"),
        "Workflow check should match the established land path: {thread_show_after_preview}"
    );
    let ready_after_preview = json(&thread_path, &["--output", "json", "ready"]);
    let parent_land_action = format!(
        "heddle --repo {} land --thread feature/ready-verify --no-push",
        temp.path().display()
    );
    assert_eq!(
        ready_after_preview["recommended_action"], parent_land_action,
        "ready rerun after preview should preserve the land path: {ready_after_preview}"
    );
    // Mutation reply no longer carries verification on the wire; the
    // injected verify is rooted in `thread_path` without --repo, so
    // its recommendation lacks the parent --repo prefix the original
    // ready built. The top-level assertion above already proves the
    // land path is preserved end-to-end.
    let parent_status_after_preview = json(temp.path(), &["--output", "json", "status"]);
    assert_eq!(
        parent_status_after_preview["recommended_action"],
        "heddle land --thread feature/ready-verify --no-push",
        "parent status should keep previewed workflow actionable: {parent_status_after_preview}"
    );
    let thread_list = json(temp.path(), &["--output", "json", "thread", "list"]);
    assert_eq!(
        thread_list["recommended_action"], "heddle land --thread feature/ready-verify --no-push",
        "thread list top-level action should match verify after preview: {thread_list}"
    );
    let listed = thread_list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|thread| thread["name"] == "feature/ready-verify")
        .expect("ready thread should be listed");
    assert_eq!(
        listed["recommended_action"], "heddle land --thread feature/ready-verify --no-push",
        "thread list should match ready/verify next action: {thread_list}"
    );
    assert_eq!(
        listed["recommended_action_template"]["argv_template"],
        heddle_argv_json(["land", "--thread", "feature/ready-verify", "--no-push"]),
        "thread list item actions should be directly executable: {thread_list}"
    );
    let workspace = json(temp.path(), &["--output", "json", "workspace", "show"]);
    assert_eq!(
        workspace["recommended_action"], "heddle land --thread feature/ready-verify --no-push",
        "workspace top-level action should match verify after preview: {workspace}"
    );
    let workspace_thread = workspace["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| group["threads"].as_array().unwrap().iter())
        .find(|thread| thread["name"] == "feature/ready-verify")
        .expect("ready thread should appear in workspace");
    assert_eq!(
        workspace_thread["recommended_action"],
        "heddle land --thread feature/ready-verify --no-push",
        "workspace should match ready/verify next action: {workspace}"
    );
    assert_eq!(
        workspace_thread["recommended_action_template"]["argv_template"],
        heddle_argv_json(["land", "--thread", "feature/ready-verify", "--no-push"]),
        "workspace item actions should be directly executable: {workspace}"
    );
}

#[test]
fn git_overlay_matrix_agent_ship_allows_absent_confidence() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle_adopt(temp.path());

    let started = json(
        temp.path(),
        &[
            "--output",
            "json",
            "start",
            "feature/land-absent-confidence",
            "--workspace",
            "materialized",
        ],
    );
    let thread_path = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());
    std::fs::write(thread_path.join("agent.txt"), "agent work\n").unwrap();
    let capture = heddle_output_with_env(
        &["capture", "-m", "agent work", "--output", "json"],
        Some(&thread_path),
        &[
            ("HEDDLE_AGENT_PROVIDER", "codex"),
            ("HEDDLE_AGENT_MODEL", "gpt-5"),
        ],
    )
    .expect("agent capture should run");
    assert!(
        capture.status.success(),
        "agent capture should succeed: {}",
        String::from_utf8_lossy(&capture.stderr)
    );

    let ready = json(&thread_path, &["--output", "json", "ready"]);
    assert_eq!(
        ready["status"], "completed",
        "absent confidence should not make ready and land disagree: {ready}"
    );
    let preview = json(
        temp.path(),
        &[
            "--output",
            "json",
            "merge",
            "feature/land-absent-confidence",
            "--preview",
        ],
    );
    assert_eq!(
        preview["recommended_action"],
        "heddle land --thread feature/land-absent-confidence --no-push"
    );

    let land = json(
        temp.path(),
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/land-absent-confidence",
            "--no-push",
        ],
    );
    assert_eq!(
        land["status"], "landed",
        "land should not block on absent confidence after ready completed: {land}"
    );
    assert_eq!(land["integrated"], true);
    assert!(
        land["blockers"]
            .as_array()
            .is_none_or(|blockers| blockers.is_empty()),
        "successful land should have no blockers: {land}"
    );
}

#[test]
fn git_overlay_matrix_low_confidence_blocks_ready_and_ship_with_recapture_action() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle_adopt(temp.path());

    let started = json(
        temp.path(),
        &[
            "--output",
            "json",
            "start",
            "feature/land-low-confidence",
            "--workspace",
            "materialized",
        ],
    );
    let thread_path = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());
    std::fs::write(thread_path.join("agent.txt"), "agent work\n").unwrap();
    let capture = heddle_output_with_env(
        &[
            "capture",
            "-m",
            "agent work",
            "--confidence",
            "0.60",
            "--output",
            "json",
        ],
        Some(&thread_path),
        &[
            ("HEDDLE_AGENT_PROVIDER", "codex"),
            ("HEDDLE_AGENT_MODEL", "gpt-5"),
        ],
    )
    .expect("agent capture should run");
    assert!(
        capture.status.success(),
        "agent capture should succeed: {}",
        String::from_utf8_lossy(&capture.stderr)
    );

    let ready = json(&thread_path, &["--output", "json", "ready"]);
    assert_eq!(ready["status"], "blocked", "{ready}");
    assert_eq!(
        ready["recommended_action"], "heddle commit -m \"...\" --confidence <confidence>",
        "ready should give the corrective action before marking the thread ready: {ready}"
    );
    assert!(
        ready["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|blocker| blocker
                .as_str()
                .is_some_and(|text| text.contains("confidence 0.60 is below"))),
        "ready should explain the confidence policy gate: {ready}"
    );

    let preview = json(
        temp.path(),
        &[
            "--output",
            "json",
            "merge",
            "feature/land-low-confidence",
            "--preview",
        ],
    );
    assert_eq!(
        preview["recommended_action"],
        "heddle land --thread feature/land-low-confidence --no-push"
    );

    let ship_output = heddle_output(
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/land-low-confidence",
            "--no-push",
        ],
        Some(temp.path()),
    )
    .expect("invoke blocked low-confidence land");
    assert!(
        !ship_output.status.success(),
        "blocked policy land should exit nonzero"
    );
    let land: Value = serde_json::from_slice(&ship_output.stdout)
        .unwrap_or_else(|err| panic!("blocked land should emit JSON on stdout: {err}"));
    assert_eq!(land["status"], "blocked");
    assert_eq!(land["integrated"], false);
    assert_eq!(land["checkpointed"], false);
    // heddle#464 bug 2: this land runs from the PARENT checkout (`temp.path()`)
    // against an isolated thread whose checkout is `thread_path`. An unscoped
    // `heddle commit` would commit the parent and never update the blocked
    // thread's confidence, so the recovery must scope the recapture to the
    // thread via the global `--repo` flag. (Contrast the in-thread `ready`
    // recovery above, which stays unscoped.)
    let expected_scoped_recapture = format!(
        "heddle --repo {} commit -m \"...\" --confidence <confidence>",
        thread_path.display()
    );
    assert_eq!(
        land["recommended_action"], expected_scoped_recapture,
        "blocked policy land from the parent must scope the recapture to the thread's checkout, not land again or commit the parent: {land}"
    );
    assert!(
        land["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|blocker| blocker
                .as_str()
                .is_some_and(|text| text.contains("confidence 0.60 is below"))),
        "land should explain the policy blocker: {land}"
    );
    assert!(
        land["skipped_steps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|step| step == "checkpoint(not reached)"),
        "blocked land should not claim checkpoint was unnecessary: {land}"
    );
    assert!(
        !land["skipped_steps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|step| step == "checkpoint(not needed)"),
        "blocked land should reserve checkpoint(not needed) for paths that reached merge: {land}"
    );
}

#[test]
fn git_overlay_matrix_ready_thread_action_not_overridden_by_remote_push() {
    let temp = TempDir::new().unwrap();
    let origin = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(&["init", "--bare", "--initial-branch=main"], origin.path());
    std::fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
    git_commit_all(temp.path(), "seed");
    let origin_arg = origin.path().to_str().expect("origin path should be utf8");
    git(&["remote", "add", "origin", origin_arg], temp.path());
    git(&["push", "-u", "origin", "main"], temp.path());
    heddle_adopt(temp.path());

    let started = json(
        temp.path(),
        &[
            "--output",
            "json",
            "start",
            "feature/stale-ready",
            "--workspace",
            "materialized",
        ],
    );
    let thread_path = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());
    std::fs::write(thread_path.join("feature.txt"), "feature\n").unwrap();
    json(
        &thread_path,
        &["--output", "json", "commit", "-m", "feature work"],
    );

    std::fs::write(temp.path().join("main.txt"), "main change\n").unwrap();
    json(
        temp.path(),
        &["--output", "json", "commit", "-m", "main work"],
    );
    let verify = json(temp.path(), &["--output", "json", "verify"]);
    assert_eq!(
        verify["recommended_action"], "heddle push",
        "fixture should have global remote-ahead guidance before scoped ready: {verify}"
    );

    let ready = json(
        temp.path(),
        &[
            "--output",
            "json",
            "ready",
            "--thread",
            "feature/stale-ready",
        ],
    );
    assert_eq!(ready["status"], "blocked", "{ready}");
    assert_eq!(
        ready["recommended_action"], "heddle sync --thread feature/stale-ready",
        "thread-scoped ready should keep the stale-thread recovery primary, not global push: {ready}"
    );
    assert_eq!(
        ready["report"]["recommended_action"], "heddle sync --thread feature/stale-ready",
        "nested report should match the top-level ready action: {ready}"
    );
    assert!(
        ready["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|blocker| blocker
                .as_str()
                .is_some_and(|text| text.contains("stale against"))),
        "ready should explain the stale-thread blocker: {ready}"
    );
}

#[test]
fn git_overlay_matrix_current_thread_recovery_not_overridden_by_remote_push() {
    let temp = TempDir::new().unwrap();
    let origin = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(&["init", "--bare", "--initial-branch=main"], origin.path());
    std::fs::write(temp.path().join("tracked.txt"), "seed\n").unwrap();
    git_commit_all(temp.path(), "seed");
    git(
        &[
            "remote",
            "add",
            "origin",
            origin.path().to_str().expect("origin path should be utf8"),
        ],
        temp.path(),
    );
    git(&["push", "-u", "origin", "main"], temp.path());
    heddle_adopt(temp.path());

    let started = json(
        temp.path(),
        &[
            "--output",
            "json",
            "start",
            "feature/stale-current",
            "--workspace",
            "materialized",
        ],
    );
    let thread_path = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());
    std::fs::write(thread_path.join("feature.txt"), "feature\n").unwrap();
    json(
        &thread_path,
        &["--output", "json", "commit", "-m", "feature work"],
    );

    std::fs::write(temp.path().join("main.txt"), "main change\n").unwrap();
    json(
        temp.path(),
        &["--output", "json", "commit", "-m", "main work"],
    );
    let verify = json(temp.path(), &["--output", "json", "verify"]);
    assert_eq!(
        verify["recommended_action"], "heddle push",
        "global clean remote-ahead guidance should remain push: {verify}"
    );

    for (label, args) in [
        ("status", vec!["--output", "json", "status"]),
        ("workspace", vec!["--output", "json", "workspace", "show"]),
        ("thread list", vec!["--output", "json", "thread", "list"]),
        (
            "thread show",
            vec![
                "--output",
                "json",
                "thread",
                "show",
                "feature/stale-current",
            ],
        ),
    ] {
        let output = json(&thread_path, &args);
        assert_eq!(
            output["recommended_action"], "heddle sync --thread feature/stale-current",
            "{label} should keep current-thread recovery primary, not global push: {output}"
        );
        assert_eq!(
            output["verification"]["recommended_action"], output["recommended_action"],
            "{label} verification should agree with the selected primary action: {output}"
        );
    }
}

#[test]
fn git_overlay_matrix_thread_and_workspace_plain_git_are_observe_only() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");

    let thread_list = json(temp.path(), &["thread", "list", "--output", "json"]);
    assert_eq!(thread_list["repository_capability"], "plain-git");
    assert_eq!(thread_list["recommended_action"], "heddle adopt --ref main");
    assert_eq!(
        thread_list["recommended_action_template"]["argv_template"],
        heddle_argv_json(["adopt", "--ref", "main"])
    );
    assert!(
        !temp.path().join(".heddle").exists(),
        "thread list in a plain Git repo must be observe-only"
    );

    let thread_show = json(temp.path(), &["thread", "show", "main", "--output", "json"]);
    assert_eq!(thread_show["repository_capability"], "plain-git");
    assert_eq!(thread_show["recommended_action"], "heddle adopt --ref main");
    assert_eq!(
        thread_show["recommended_action_template"]["argv_template"],
        heddle_argv_json(["adopt", "--ref", "main"])
    );
    assert!(
        !temp.path().join(".heddle").exists(),
        "thread show in a plain Git repo must be observe-only"
    );

    let workspace = json(temp.path(), &["workspace", "show", "--output", "json"]);
    assert_eq!(workspace["repository_capability"], "plain-git");
    assert_eq!(workspace["verification"]["status"], "needs_init");
    assert_eq!(workspace["recommended_action"], "heddle adopt --ref main");
    assert_eq!(
        workspace["recommended_action_template"]["argv_template"],
        heddle_argv_json(["adopt", "--ref", "main"])
    );
    assert_verify_check_rows(&workspace["verification"]);
    assert!(
        !temp.path().join(".heddle").exists(),
        "workspace show in a plain Git repo must be observe-only"
    );
}

#[test]
fn git_overlay_matrix_observe_only_contract_preserves_plain_git_repo() {
    let catalog: Value =
        serde_json::from_str(&heddle(&["commands", "--output", "json"], None).unwrap())
            .expect("command catalog should be JSON");
    let commands = catalog["commands"]
        .as_array()
        .expect("catalog commands should be an array");
    let cases: &[(&str, &[&str])] = &[
        ("status", &["status", "--output", "json"]),
        ("doctor", &["doctor", "--output", "json"]),
        ("doctor", &["doctor", "--output", "json"]),
        (
            "bridge git status",
            &["bridge", "git", "status", "--output", "json"],
        ),
        ("verify", &["verify", "--output", "json"]),
        ("thread list", &["thread", "list", "--output", "json"]),
        (
            "thread show",
            &["thread", "show", "main", "--output", "json"],
        ),
        ("workspace show", &["workspace", "show", "--output", "json"]),
        ("log", &["log", "--output", "json"]),
        ("show", &["show", "HEAD", "--output", "json"]),
        ("diff", &["diff", "--output", "json"]),
        ("remote list", &["remote", "list", "--output", "json"]),
        (
            "remote show",
            &["remote", "show", "origin", "--output", "json"],
        ),
    ];

    for (display, args) in cases {
        let entry = commands
            .iter()
            .find(|entry| entry["display"] == *display)
            .unwrap_or_else(|| panic!("missing command catalog entry for `{display}`"));
        assert_eq!(
            entry["observe_only"], true,
            "`{display}` must be observe_only in the command contract table"
        );
        assert_eq!(
            entry["mutates"], false,
            "`{display}` must not mutate in the command contract table"
        );

        let temp = TempDir::new().unwrap();
        init_git_repo_with_branch(temp.path(), "main");
        std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
        git_commit_all(temp.path(), "seed");
        std::fs::write(temp.path().join("pending.txt"), "pending\n").unwrap();

        let before_status = git_status_short(temp.path());
        let before_refs = git_ref_snapshot(temp.path());
        let output = heddle_output(args, Some(temp.path()))
            .unwrap_or_else(|err| panic!("heddle {:?} should execute: {}", args, err));
        let after_status = git_status_short(temp.path());
        let after_refs = git_ref_snapshot(temp.path());

        assert!(
            !temp.path().join(".heddle").exists(),
            "`{display}` must not create .heddle in a plain Git repo; status: {:?}, stderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            after_status, before_status,
            "`{display}` must not change git status --short"
        );
        assert_eq!(
            after_refs, before_refs,
            "`{display}` must not move or create Git refs"
        );
    }
}

#[test]
fn git_overlay_matrix_native_bridge_import_materializes_current_thread_when_clean() {
    let source = TempDir::new().unwrap();
    init_git_repo_with_branch(source.path(), "main");
    std::fs::write(source.path().join("README.md"), "imported\n").unwrap();
    git_commit_all(source.path(), "seed");

    let dest = TempDir::new().unwrap();
    heddle(&["init"], Some(dest.path())).unwrap();
    let source_arg = source.path().to_str().expect("source path should be utf8");
    let import = json(
        dest.path(),
        &[
            "--output", "json", "bridge", "git", "import", "--path", source_arg, "--ref", "main",
        ],
    );
    assert_eq!(import["states_created"], 1);
    assert_eq!(
        std::fs::read_to_string(dest.path().join("README.md")).unwrap(),
        "imported\n",
        "native import into the current thread should materialize the imported tree"
    );
    let verify = json(dest.path(), &["--output", "json", "verify"]);
    assert_eq!(verify["verified"], true, "{verify}");
    assert_eq!(verify["status"], "clean", "{verify}");
}

#[test]
fn git_overlay_matrix_init_in_git_repo_keeps_git_status_clean() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");

    heddle(&["init"], Some(temp.path())).unwrap();
    assert!(
        !temp.path().join(".heddleignore").exists(),
        "git-overlay init should not create a tracked root .heddleignore"
    );

    let output = Command::new("git")
        .args(["status", "--short"])
        .current_dir(temp.path())
        .output()
        .expect("git status should run");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "",
        "init should leave the Git checkout clean"
    );
}

#[test]
fn git_overlay_matrix_init_excludes_only_heddle_metadata() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");
    std::fs::create_dir_all(temp.path().join("src/__pycache__")).unwrap();
    std::fs::write(
        temp.path().join("src/__pycache__/app.cpython-312.pyc"),
        "cache",
    )
    .unwrap();
    std::fs::write(temp.path().join("src/app.pyc"), "cache").unwrap();

    let before = Command::new("git")
        .args(["status", "--short"])
        .current_dir(temp.path())
        .output()
        .expect("git status should run");
    assert!(
        String::from_utf8_lossy(&before.stdout).contains("src/"),
        "fixture should start with raw Git reporting generated noise"
    );

    heddle(&["init"], Some(temp.path())).unwrap();
    assert!(
        !temp.path().join(".heddleignore").exists(),
        "git-overlay init should not create a tracked root .heddleignore"
    );

    let exclude = std::fs::read_to_string(temp.path().join(".git/info/exclude")).unwrap();
    {
        let pattern = ".heddle/";
        assert!(
            exclude.lines().any(|line| line.trim() == pattern),
            "local Git exclude should contain {pattern:?}: {exclude}"
        );
    }
    for pattern in [
        ".heddleignore",
        "__pycache__",
        "*.pyc",
        ".pytest_cache",
        ".mypy_cache",
        ".ruff_cache",
    ] {
        assert!(
            !exclude.lines().any(|line| line.trim() == pattern),
            "local Git exclude should not auto-ignore project artifacts with {pattern:?}: {exclude}"
        );
    }

    let after = Command::new("git")
        .args(["status", "--short"])
        .current_dir(temp.path())
        .output()
        .expect("git status should run");
    assert!(after.status.success());
    assert!(
        String::from_utf8_lossy(&after.stdout).contains("src/"),
        "unignored generated noise should remain visible to raw Git after init"
    );
}

#[test]
fn git_overlay_matrix_reconcile_apply_imports_current_git_branch() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle(&["init"], Some(temp.path())).unwrap();

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["git_overlay_health"]["status"], "needs_import");
    assert_eq!(status["recommended_action"], "heddle adopt --ref main");

    let reconcile = json(
        temp.path(),
        &[
            "bridge",
            "git",
            "reconcile",
            "--prefer",
            "git",
            "--ref",
            "main",
        ],
    );
    assert_eq!(reconcile["status"], "completed");

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["git_overlay_health"]["status"], "clean");
    assert_eq!(status["thread"], "main");
}

#[test]
fn git_overlay_matrix_reconcile_prefer_heddle_missing_thread_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &[
            "--output",
            "json",
            "bridge",
            "git",
            "reconcile",
            "--prefer",
            "heddle",
            "--ref",
            "main",
        ],
        Some(temp.path()),
    )
    .expect("invoke reconcile");
    assert!(
        !output.status.success(),
        "preferring a missing Heddle thread should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode reconcile refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("missing Heddle thread should emit JSON envelope");
    assert_eq!(envelope["kind"], "reconcile_missing_heddle_thread");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("no matching Heddle thread exists")),
        "reconcile refusal should include full typed advice: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle adopt --ref main")
                && hint.contains("heddle bridge git reconcile --prefer git --ref main")),
        "reconcile hint should offer import and prefer-git recovery: {stderr}"
    );
}

#[test]
fn git_overlay_matrix_commit_ignores_gitignored_noise_and_refuses_noop() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join(".gitignore"), "__pycache__/\n*.pyc\n").unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(
        &["bridge", "git", "import", "--ref", "main"],
        Some(temp.path()),
    )
    .unwrap();

    std::fs::create_dir(temp.path().join("__pycache__")).unwrap();
    std::fs::write(temp.path().join("__pycache__/tracked.pyc"), "cache").unwrap();

    let output = heddle_output(
        &["--output", "json", "commit", "-m", "noop"],
        Some(temp.path()),
    )
    .expect("commit should run");
    assert!(!output.status.success(), "ignored-only commit should fail");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode no-op commit refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: serde_json::Value =
        serde_json::from_str(stderr).expect("no-op commit should emit JSON envelope");
    assert_eq!(envelope["kind"], "nothing_to_commit");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("nothing to commit")),
        "ignored-only commit should refuse with full typed advice: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle status")),
        "ignored-only commit should name the recovery command: {stderr}"
    );
}

#[test]
fn git_overlay_matrix_commit_requires_explicit_ignore_for_python_generated_noise() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(
        &["bridge", "git", "import", "--ref", "main"],
        Some(temp.path()),
    )
    .unwrap();

    std::fs::create_dir_all(temp.path().join("src/__pycache__")).unwrap();
    std::fs::write(
        temp.path().join("src/__pycache__/app.cpython-312.pyc"),
        "cache",
    )
    .unwrap();
    std::fs::write(temp.path().join("src/app.pyc"), "cache").unwrap();

    let status_output = heddle_output(&["--output", "json", "status"], Some(temp.path()))
        .expect("status should run");
    assert!(status_output.status.success());
    let status: serde_json::Value =
        serde_json::from_slice(&status_output.stdout).expect("status should be JSON");
    let will_commit = status["git_index"]["will_commit"]
        .as_array()
        .expect("will_commit array");
    assert!(
        will_commit
            .iter()
            .any(|path| path == "src/__pycache__/app.cpython-312.pyc"),
        "unignored generated noise must stay visible in the Git index plan: {status}"
    );
    assert!(
        will_commit.iter().any(|path| path == "src/app.pyc"),
        "unignored generated noise must stay visible in the Git index plan: {status}"
    );

    let output = heddle_output(
        &["--output", "json", "commit", "-m", "capture generated"],
        Some(temp.path()),
    )
    .expect("commit should run");
    assert!(
        output.status.success(),
        "unignored generated files should be committed unless the repo explicitly ignores them: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let commit: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("commit should emit JSON");
    assert_eq!(commit["status"], "committed");
}

#[test]
fn git_overlay_matrix_commit_noop_fails_closed_when_verification_blocked() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(
        &["bridge", "git", "import", "--ref", "main"],
        Some(temp.path()),
    )
    .unwrap();

    let head = git_stdout(temp.path(), &["rev-parse", "HEAD"]);
    std::fs::write(
        temp.path().join(".git").join("MERGE_HEAD"),
        format!("{head}\n"),
    )
    .unwrap();

    let output = heddle_output(
        &["--output", "json", "commit", "-m", "noop"],
        Some(temp.path()),
    )
    .expect("commit should run");
    assert!(
        !output.status.success(),
        "verified-blocked commit should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode verified-blocked commit refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: serde_json::Value =
        serde_json::from_str(stderr).expect("verify-blocked commit should emit JSON envelope");
    assert_eq!(envelope["kind"], "raw_git_operation_in_progress");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| { error.contains("externally-started Git merge is in progress") }),
        "verify-blocked no-op commit should refuse with full typed advice: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle bridge git status")
                && hint.contains("finish or abort it with the Git-compatible tool")),
        "verify-blocked no-op commit should name the verify recovery command: {stderr}"
    );
}

#[test]
fn git_overlay_matrix_undo_rewinds_git_checkpoint_when_safe() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "one\n").unwrap();
    git_commit_all(temp.path(), "seed");
    let base = git_stdout(temp.path(), &["rev-parse", "HEAD"]);
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(
        &["bridge", "git", "import", "--ref", "main"],
        Some(temp.path()),
    )
    .unwrap();

    std::fs::write(temp.path().join("tracked.txt"), "two\n").unwrap();
    let commit = json(temp.path(), &["--output", "json", "commit", "-m", "change"]);
    assert_eq!(commit["output_kind"], "commit");
    assert!(commit["git_commit"].as_str().is_some());
    let after = git_stdout(temp.path(), &["rev-parse", "HEAD"]);
    assert_ne!(after, base);

    let undo_list = json(
        temp.path(),
        &["--output", "json", "undo", "--list", "--depth", "1"],
    );
    let operations = undo_list["batches"][0]["operations"]
        .as_array()
        .expect("undo list should expose operations");
    let logical_operations: Vec<_> = operations
        .iter()
        .filter(|op| {
            !op["description"]
                .as_str()
                .is_some_and(|description| description.starts_with("transaction commit "))
        })
        .collect();
    assert_eq!(
        logical_operations.len(),
        2,
        "compat commit should be one logical undo batch containing capture + Git checkpoint: {undo_list}"
    );
    assert!(
        logical_operations.iter().any(|op| op["description"]
            .as_str()
            .is_some_and(|description| description.starts_with("snapshot "))),
        "commit undo batch should include the captured Heddle state: {undo_list}"
    );
    assert!(
        logical_operations.iter().any(|op| op["description"]
            .as_str()
            .is_some_and(|description| description.starts_with("git checkpoint "))),
        "commit undo batch should include the Git checkpoint: {undo_list}"
    );

    let undo = json(temp.path(), &["--output", "json", "undo"]);
    assert_eq!(undo["action"], "undo");
    assert_eq!(git_stdout(temp.path(), &["rev-parse", "HEAD"]), base);
    assert_eq!(
        git_stdout(temp.path(), &["reflog", "-1", "--format=%gs", "HEAD"]),
        "heddle: undo git checkpoint",
        "undo should update the visible HEAD reflog, not only refs/heads/main"
    );
    assert_eq!(
        git_stdout(
            temp.path(),
            &["reflog", "-1", "--format=%gs", "refs/heads/main"]
        ),
        "heddle: undo git checkpoint",
        "undo should keep the branch reflog aligned with HEAD"
    );
    assert_eq!(
        mirror_git_stdout(temp.path(), &["rev-parse", "refs/heads/main"]),
        base,
        "undo should rewind the internal Git mirror branch as well as the visible Git checkout"
    );
    assert_eq!(git_stdout(temp.path(), &["status", "--short"]), "");
    let verify = json(temp.path(), &["--output", "json", "verify"]);
    assert_eq!(
        undo["verification"]["status"], verify["status"],
        "undo JSON verify status should match an immediate verify probe: undo={undo}, verify={verify}"
    );
    assert_eq!(
        undo["verification"]["recommended_action"], verify["recommended_action"],
        "undo JSON recommended action should match an immediate verify probe: undo={undo}, verify={verify}"
    );
    let status = json(temp.path(), &["--output", "json", "status"]);
    assert_eq!(status["git_overlay_health"]["status"], "clean");

    std::fs::write(temp.path().join("tracked.txt"), "three\n").unwrap();
    let second = json(
        temp.path(),
        &["--output", "json", "commit", "-m", "change after undo"],
    );
    assert_eq!(second["output_kind"], "commit");
    assert!(
        second["git_commit"]
            .as_str()
            .is_some_and(|git_commit| git_commit != after),
        "a new commit after undo should checkpoint normally, not try to rewrite the undone export: {second}"
    );
}

#[test]
fn git_overlay_matrix_undo_after_push_recommends_publish_undo_not_pull() {
    let origin = TempDir::new().unwrap();
    git(&["init", "--bare", "--initial-branch=main"], origin.path());

    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(
        &["remote", "add", "origin", origin.path().to_str().unwrap()],
        temp.path(),
    );
    std::fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
    git_commit_all(temp.path(), "seed");
    let base_git = git_stdout(temp.path(), &["rev-parse", "HEAD"]);
    git(&["push", "-u", "origin", "main"], temp.path());
    heddle_adopt(temp.path());

    std::fs::write(temp.path().join("tracked.txt"), "published change\n").unwrap();
    let commit = json(
        temp.path(),
        &["--output", "json", "commit", "-m", "published change"],
    );
    let published_git = commit["git_commit"].as_str().unwrap().to_string();
    assert_ne!(published_git, base_git);

    let push = json(temp.path(), &["--output", "json", "push", "origin"]);
    assert_eq!(push["verification"]["verified"], true, "{push}");
    assert_eq!(
        git_stdout(origin.path(), &["rev-parse", "refs/heads/main"]),
        published_git
    );

    let undo = json(temp.path(), &["--output", "json", "undo"]);
    assert_eq!(git_stdout(temp.path(), &["rev-parse", "HEAD"]), base_git);
    assert_eq!(
        undo["verification"]["status"], "remote_contains_undone_checkpoint",
        "undo should classify the upstream as the just-undone checkpoint: {undo}"
    );
    assert_eq!(
        undo["verification"]["recommended_action"], "heddle push --force",
        "undo must not recommend pulling the change the user just undid: {undo}"
    );
    assert_eq!(
        undo["verification"]["recommended_action_template"]["argv_template"],
        heddle_argv_json(["push", "--force"]),
        "agents should receive the same publish-undo action as structured argv: {undo}"
    );
    assert!(
        undo["verification"]["recovery_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command == "heddle redo"),
        "undo should also name the restore-the-work option: {undo}"
    );

    let status = json(temp.path(), &["--output", "json", "status"]);
    assert_eq!(
        status["remote_tracking"]["next_action"], "heddle push --force",
        "raw remote tracking guidance must agree with verification: {status}"
    );
    assert_eq!(
        status["verification"]["remote_drift"],
        "remote_contains_undone_checkpoint"
    );
    assert_eq!(status["recommended_action"], "heddle push --force");
    assert_eq!(
        status["recommended_action_template"]["argv_template"],
        heddle_argv_json(["push", "--force"])
    );

    let publish_undo = json(
        temp.path(),
        &["--output", "json", "push", "origin", "--force"],
    );
    assert_eq!(publish_undo["force"], true);
    assert!(
        publish_undo["force_discard_warning"]
            .as_str()
            .is_some_and(|warning| warning.contains("discarded")),
        "force push should state the remote discard risk: {publish_undo}"
    );
    assert_eq!(
        publish_undo["verification"]["verified"], true,
        "{publish_undo}"
    );
    assert_eq!(
        git_stdout(origin.path(), &["rev-parse", "refs/heads/main"]),
        base_git,
        "force-publishing the undo should move the remote back to the local branch"
    );
}

#[test]
fn git_overlay_matrix_merge_git_commit_fast_forward_records_checkpoint_and_undoes_together() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("README.md"), "base\n").unwrap();
    git_commit_all(temp.path(), "base");
    let base_git = git_stdout(temp.path(), &["rev-parse", "HEAD"]);
    heddle_adopt(temp.path());

    let feature_path = temp.path().with_extension("feature-merge-git-commit-ff");
    json(
        temp.path(),
        &[
            "--output",
            "json",
            "start",
            "feature/merge-git-commit-ff",
            "--path",
            feature_path.to_str().unwrap(),
        ],
    );
    std::fs::write(feature_path.join("sibling.txt"), "sibling\n").unwrap();
    heddle(&["capture", "-m", "sibling captured"], Some(&feature_path)).unwrap();

    let merge = json(
        temp.path(),
        &[
            "--output",
            "json",
            "merge",
            "feature/merge-git-commit-ff",
            "-m",
            "merge sibling thread",
            "--git-commit",
        ],
    );
    assert_eq!(merge["status"], "completed");
    assert_eq!(merge["fast_forward"], true);
    assert!(merge["git_commit"]["sha"].as_str().is_some());
    assert_eq!(merge["verification"]["verified"], true);
    assert_eq!(merge["verification"]["status"], "clean");

    let verify = json(temp.path(), &["--output", "json", "verify"]);
    assert_eq!(verify["verified"], true);
    assert_eq!(verify["status"], "clean");
    assert_eq!(verify["mapping_state"], "clean");
    assert_eq!(git_status_short(temp.path()), "");

    let undo_list = json(
        temp.path(),
        &["--output", "json", "undo", "--list", "--depth", "1"],
    );
    let operations = undo_list["batches"][0]["operations"]
        .as_array()
        .expect("undo list should expose operations");
    assert_eq!(
        operations.len(),
        2,
        "merge --git-commit should be one logical undo batch containing merge + Git checkpoint: {undo_list}"
    );
    assert!(
        operations.iter().any(|op| op["description"]
            .as_str()
            .is_some_and(|description| description.starts_with("fast-forward "))),
        "fast-forward merge batch should include the Heddle merge movement: {undo_list}"
    );
    assert!(
        operations.iter().any(|op| op["description"]
            .as_str()
            .is_some_and(|description| description.starts_with("git checkpoint "))),
        "fast-forward merge batch should include the Git checkpoint: {undo_list}"
    );

    let undo = json(temp.path(), &["--output", "json", "undo"]);
    assert_eq!(undo["status"], "completed");
    assert_eq!(undo["verification"]["verified"], true);
    assert_eq!(undo["verification"]["status"], "clean");
    assert_eq!(git_stdout(temp.path(), &["rev-parse", "HEAD"]), base_git);
    assert_eq!(git_status_short(temp.path()), "");
    assert!(!temp.path().join("sibling.txt").exists());
}

#[test]
fn git_overlay_matrix_merge_git_commit_fast_forward_uses_git_merge_checkpoint_when_branch_exists() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("README.md"), "base\n").unwrap();
    git_commit_all(temp.path(), "base");
    heddle_adopt(temp.path());

    json(
        temp.path(),
        &["--output", "json", "branch", "feature/merge-sample"],
    );
    json(
        temp.path(),
        &["--output", "json", "switch", "feature/merge-sample"],
    );
    std::fs::write(temp.path().join("feature.txt"), "feature\n").unwrap();
    let feature_commit = json(
        temp.path(),
        &["--output", "json", "commit", "-m", "Add feature file"],
    );
    assert_eq!(feature_commit["verification"]["verified"], true);
    let feature_git = git_stdout(temp.path(), &["rev-parse", "feature/merge-sample"]);

    json(temp.path(), &["--output", "json", "switch", "main"]);
    let preview_text = heddle(
        &[
            "--output",
            "text",
            "merge",
            "feature/merge-sample",
            "--git-commit",
            "--preview",
        ],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        preview_text.contains("Would advance main"),
        "preview should describe Heddle movement plus Git checkpoint honestly: {preview_text}"
    );
    assert!(
        !preview_text.contains("Would fast-forward"),
        "preview must not imply a Git graph fast-forward when --git-commit will write a checkpoint: {preview_text}"
    );

    let merge = json(
        temp.path(),
        &[
            "--output",
            "json",
            "merge",
            "feature/merge-sample",
            "--git-commit",
        ],
    );
    assert_eq!(merge["status"], "completed");
    assert_eq!(merge["fast_forward"], true);
    assert!(
        merge["message"]
            .as_str()
            .is_some_and(|message| message.contains("wrote a Git checkpoint commit")),
        "JSON should describe the Git side as a checkpoint commit, not a graph fast-forward: {merge}"
    );
    assert!(
        !merge["message"]
            .as_str()
            .is_some_and(|message| message.contains("Fast-forwarded")),
        "JSON must not claim a Git graph fast-forward: {merge}"
    );
    let git_commit = merge["git_commit"]["sha"]
        .as_str()
        .expect("merge should write a Git commit");
    let parents = git_stdout(temp.path(), &["log", "-1", "--pretty=%P"]);
    assert!(
        parents
            .split_whitespace()
            .any(|parent| parent == feature_git),
        "Git checkpoint should include the source branch tip as a parent so Git agrees it is merged: commit={git_commit}, parents={parents}, source={feature_git}"
    );
    git(&["branch", "-d", "feature/merge-sample"], temp.path());
}

#[test]
fn git_overlay_matrix_push_preserves_merge_git_checkpoint_tip() {
    let temp = TempDir::new().unwrap();
    let origin = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(&["init", "--bare", "--initial-branch=main"], origin.path());
    git(
        &[
            "remote",
            "add",
            "origin",
            origin.path().to_str().expect("origin path should be utf8"),
        ],
        temp.path(),
    );
    std::fs::write(temp.path().join("README.md"), "base\n").unwrap();
    git_commit_all(temp.path(), "base");
    heddle_adopt(temp.path());

    json(
        temp.path(),
        &["--output", "json", "branch", "feature/audit"],
    );
    json(
        temp.path(),
        &["--output", "json", "switch", "feature/audit"],
    );
    std::fs::write(temp.path().join("audit.txt"), "thread edit\n").unwrap();
    json(
        temp.path(),
        &["--output", "json", "commit", "-m", "thread edit"],
    );
    json(temp.path(), &["--output", "json", "switch", "main"]);

    let merge = json(
        temp.path(),
        &[
            "--output",
            "json",
            "merge",
            "feature/audit",
            "-m",
            "merge thread audit",
            "--git-commit",
        ],
    );
    assert_eq!(merge["status"], "completed");
    let merge_sha = git_stdout(temp.path(), &["rev-parse", "HEAD"]);
    assert_eq!(
        git_stdout(temp.path(), &["log", "-1", "--format=%s"]),
        "merge thread audit"
    );

    let push = json(temp.path(), &["--output", "json", "push", "origin"]);
    assert_eq!(push["success"], true);
    assert_eq!(
        git_stdout(temp.path(), &["rev-parse", "HEAD"]),
        merge_sha,
        "push must not rewrite the visible local Git checkpoint commit"
    );
    assert_eq!(
        git_stdout(temp.path(), &["log", "-1", "--format=%s"]),
        "merge thread audit",
        "push must preserve the user-supplied merge checkpoint message"
    );
    assert_eq!(
        mirror_git_stdout(temp.path(), &["rev-parse", "refs/heads/main"]),
        merge_sha,
        "the bridge mirror should push the checkpoint commit, not a synthesized export"
    );
    assert_eq!(
        git_stdout(origin.path(), &["rev-parse", "refs/heads/main"]),
        merge_sha,
        "the remote should receive the same checkpoint commit visible locally"
    );
}

#[test]
fn git_overlay_matrix_merge_git_commit_three_way_records_checkpoint_and_undoes_together() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("README.md"), "base\n").unwrap();
    git_commit_all(temp.path(), "base");
    heddle_adopt(temp.path());

    let feature_path = temp.path().with_extension("feature-merge-git-commit-3way");
    json(
        temp.path(),
        &[
            "--output",
            "json",
            "start",
            "feature/merge-git-commit-3way",
            "--path",
            feature_path.to_str().unwrap(),
        ],
    );
    std::fs::write(feature_path.join("feature.txt"), "feature\n").unwrap();
    heddle(&["capture", "-m", "feature captured"], Some(&feature_path)).unwrap();

    std::fs::write(temp.path().join("main.txt"), "main\n").unwrap();
    let main_commit = json(
        temp.path(),
        &["--output", "json", "commit", "-m", "main change"],
    );
    assert_eq!(main_commit["verification"]["verified"], true);
    let main_git = git_stdout(temp.path(), &["rev-parse", "HEAD"]);
    let refresh = json(
        temp.path(),
        &[
            "--output",
            "json",
            "thread",
            "refresh",
            "feature/merge-git-commit-3way",
        ],
    );
    assert_eq!(refresh["status"], "completed", "{refresh}");

    let merge = json(
        temp.path(),
        &[
            "--output",
            "json",
            "merge",
            "feature/merge-git-commit-3way",
            "-m",
            "merge feature thread",
            "--git-commit",
        ],
    );
    assert_eq!(merge["status"], "completed");
    assert_eq!(merge["fast_forward"], true);
    assert!(merge["git_commit"]["sha"].as_str().is_some());
    assert_eq!(merge["verification"]["verified"], true);
    assert_eq!(merge["verification"]["status"], "clean");

    let undo_list = json(
        temp.path(),
        &["--output", "json", "undo", "--list", "--depth", "1"],
    );
    let operations = undo_list["batches"][0]["operations"]
        .as_array()
        .expect("undo list should expose operations");
    assert_eq!(
        operations.len(),
        2,
        "refreshed merge --git-commit should be one logical undo batch containing merge state + Git checkpoint: {undo_list}"
    );
    assert!(
        operations.iter().any(|op| op["description"]
            .as_str()
            .is_some_and(|description| description
                .starts_with("fast-forward feature/merge-git-commit-3way into main"))),
        "refreshed merge batch should include the Heddle fast-forward state: {undo_list}"
    );
    assert!(
        operations.iter().any(|op| op["description"]
            .as_str()
            .is_some_and(|description| description.starts_with("git checkpoint "))),
        "3-way merge batch should include the Git checkpoint: {undo_list}"
    );

    let undo = json(temp.path(), &["--output", "json", "undo"]);
    assert_eq!(undo["status"], "completed");
    assert_eq!(undo["verification"]["verified"], true);
    assert_eq!(undo["verification"]["status"], "clean");
    assert_eq!(git_stdout(temp.path(), &["rev-parse", "HEAD"]), main_git);
    assert_eq!(git_status_short(temp.path()), "");
    assert!(temp.path().join("main.txt").exists());
    assert!(!temp.path().join("feature.txt").exists());
}

#[test]
fn git_overlay_matrix_undo_text_reports_non_clean_post_verify_next_action() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "one\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(
        &["bridge", "git", "import", "--ref", "main"],
        Some(temp.path()),
    )
    .unwrap();

    std::fs::write(temp.path().join("tracked.txt"), "two\n").unwrap();
    heddle(&["capture", "-m", "captured"], Some(temp.path())).unwrap();
    heddle(&["checkpoint", "-m", "checkpointed"], Some(temp.path())).unwrap();

    let undo = heddle(&["undo", "--output", "text"], Some(temp.path())).unwrap();
    let verify = json(temp.path(), &["--output", "json", "verify"]);
    assert_ne!(
        verify["status"], "clean",
        "checkpoint undo should leave a non-clean verify state for text UX coverage: {verify}"
    );
    let expected_status = verify["status"].as_str().unwrap();
    let expected_action = verify["recommended_action"].as_str().unwrap();
    let expected_status_text = match expected_status {
        "dirty_worktree" | "uncaptured" => "changes to save",
        other => other,
    };
    assert!(
        undo.contains(&format!("Verification: {expected_status_text}")),
        "undo text should name the current post-undo verify status: {undo}"
    );
    assert!(
        undo.contains(&format!("Next: {expected_action}")),
        "undo text should name the primary post-undo next action: {undo}"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("tracked.txt")).unwrap(),
        "two\n",
        "undoing only the Git checkpoint should keep the worktree aligned with the current Heddle state"
    );
    assert!(
        git_status_short(temp.path()).contains("tracked.txt"),
        "Git should now see the saved Heddle state as checkpoint-needed work: {}",
        git_status_short(temp.path())
    );
    let status_text = heddle(&["status", "--output", "text"], Some(temp.path())).unwrap();
    assert!(
        status_text.contains("checkpoint needed") && !status_text.contains("Git: saved to commit"),
        "status should not claim the current saved state is still checkpointed after undo removed the Git commit: {status_text}"
    );
    let status_json = json(temp.path(), &["--output", "json", "status"]);
    assert_eq!(status_json["git_checkpoint"], Value::Null);

    let second_undo = json(temp.path(), &["--output", "json", "undo"]);
    assert_eq!(second_undo["status"], "completed");
    assert_eq!(
        std::fs::read_to_string(temp.path().join("tracked.txt")).unwrap(),
        "one\n",
        "the next undo should be able to undo the capture without a false dirty-worktree refusal"
    );
    assert_eq!(git_status_short(temp.path()), "");
}

#[test]
fn git_overlay_matrix_undo_preview_refuses_dirty_worktree_like_real_undo() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "one\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(
        &["bridge", "git", "import", "--ref", "main"],
        Some(temp.path()),
    )
    .unwrap();

    std::fs::write(temp.path().join("tracked.txt"), "two\n").unwrap();
    let commit = json(temp.path(), &["--output", "json", "commit", "-m", "change"]);
    assert_eq!(commit["output_kind"], "commit");
    let git_before = git_ref_snapshot(temp.path());
    let heddle_before = json(temp.path(), &["--output", "json", "status"])["current_state"]
        .as_str()
        .expect("status should report current Heddle state")
        .to_string();

    std::fs::write(temp.path().join("tracked.txt"), "dirty after commit\n").unwrap();

    let preview = heddle_output(
        &["--output", "json", "undo", "--preview"],
        Some(temp.path()),
    )
    .expect("undo preview should run");
    assert!(
        !preview.status.success(),
        "dirty undo preview should refuse"
    );
    assert!(
        preview.stdout.is_empty(),
        "JSON-mode dirty undo preview must keep stdout quiet: {}",
        String::from_utf8_lossy(&preview.stdout)
    );
    let preview_stderr = std::str::from_utf8(&preview.stderr).unwrap();
    let preview_envelope: Value =
        serde_json::from_str(preview_stderr).expect("dirty preview should emit JSON envelope");
    assert_eq!(preview_envelope["kind"], "dirty_worktree");
    assert_json_recovery_advice_fields(&preview_envelope, &preview_envelope.to_string());
    assert!(
        preview_envelope["unsafe_condition"]
            .as_str()
            .is_some_and(|condition| condition.contains("modified: tracked.txt")),
        "dirty preview should name dirty paths: {preview_stderr}"
    );
    assert_eq!(
        git_ref_snapshot(temp.path()),
        git_before,
        "dirty preview refusal must not move Git refs"
    );
    assert_eq!(
        json(temp.path(), &["--output", "json", "status"])["current_state"],
        heddle_before,
        "dirty preview refusal must leave Heddle state untouched"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("tracked.txt")).unwrap(),
        "dirty after commit\n",
        "dirty preview refusal must leave the user's worktree bytes untouched"
    );

    let undo =
        heddle_output(&["--output", "json", "undo"], Some(temp.path())).expect("undo should run");
    assert!(!undo.status.success(), "dirty real undo should refuse");
    let undo_stderr = std::str::from_utf8(&undo.stderr).unwrap();
    let undo_envelope: Value =
        serde_json::from_str(undo_stderr).expect("dirty undo should emit JSON envelope");
    assert_eq!(undo_envelope["kind"], preview_envelope["kind"]);
    assert_eq!(
        undo_envelope["primary_command"], preview_envelope["primary_command"],
        "preview and real undo should share recovery advice"
    );
    assert_eq!(git_ref_snapshot(temp.path()), git_before);
    assert_eq!(
        json(temp.path(), &["--output", "json", "status"])["current_state"],
        heddle_before
    );
}

#[test]
fn git_overlay_matrix_undo_preview_refuses_active_operation_like_real_undo() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "one\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(
        &["bridge", "git", "import", "--ref", "main"],
        Some(temp.path()),
    )
    .unwrap();

    std::fs::write(temp.path().join("tracked.txt"), "two\n").unwrap();
    let commit = json(temp.path(), &["--output", "json", "commit", "-m", "change"]);
    assert_eq!(commit["output_kind"], "commit");

    seed_heddle_bisect_state(temp.path());
    let git_before = git_ref_snapshot(temp.path());
    let status_before = json(temp.path(), &["--output", "json", "status"]);
    assert_eq!(status_before["operation"]["kind"], "bisect");
    let heddle_before = status_before["current_state"]
        .as_str()
        .expect("status should report current Heddle state")
        .to_string();

    let preview = heddle_output(
        &["--output", "json", "undo", "--preview"],
        Some(temp.path()),
    )
    .expect("undo preview should run");
    assert!(
        !preview.status.success(),
        "active-operation undo preview should refuse"
    );
    assert!(
        preview.stdout.is_empty(),
        "JSON-mode active-operation undo preview must keep stdout quiet: {}",
        String::from_utf8_lossy(&preview.stdout)
    );
    let preview_stderr = std::str::from_utf8(&preview.stderr).unwrap();
    let preview_envelope: Value = serde_json::from_str(preview_stderr)
        .expect("active-operation preview should emit JSON envelope");
    assert_eq!(preview_envelope["kind"], "operation_in_progress");
    assert_json_recovery_advice_fields(&preview_envelope, &preview_envelope.to_string());
    assert!(
        preview_envelope["unsafe_condition"]
            .as_str()
            .is_some_and(|condition| condition.contains("heddle bisect is in-progress")),
        "active-operation preview should name the operation: {preview_stderr}"
    );
    assert_eq!(
        preview_envelope["primary_command"],
        "heddle abort"
    );
    assert_eq!(
        git_ref_snapshot(temp.path()),
        git_before,
        "active-operation preview refusal must not move Git refs"
    );
    let status_after_preview = json(temp.path(), &["--output", "json", "status"]);
    assert_eq!(status_after_preview["current_state"], heddle_before);
    assert_eq!(
        status_after_preview["operation"]["kind"], "bisect",
        "preview refusal must leave the active operation in place"
    );

    let undo =
        heddle_output(&["--output", "json", "undo"], Some(temp.path())).expect("undo should run");
    assert!(
        !undo.status.success(),
        "active-operation real undo should refuse"
    );
    let undo_stderr = std::str::from_utf8(&undo.stderr).unwrap();
    let undo_envelope: Value =
        serde_json::from_str(undo_stderr).expect("active-operation undo should emit JSON envelope");
    assert_eq!(undo_envelope["kind"], preview_envelope["kind"]);
    assert_eq!(
        undo_envelope["primary_command"], preview_envelope["primary_command"],
        "preview and real undo should share recovery advice"
    );
    assert_eq!(git_ref_snapshot(temp.path()), git_before);
}

#[test]
fn git_overlay_matrix_unsafe_commit_undo_reports_git_oid_and_preserves_heddle() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "one\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(
        &["bridge", "git", "import", "--ref", "main"],
        Some(temp.path()),
    )
    .unwrap();

    std::fs::write(temp.path().join("tracked.txt"), "two\n").unwrap();
    let commit = json(temp.path(), &["--output", "json", "commit", "-m", "change"]);
    let expected_git = commit["git_commit"]
        .as_str()
        .expect("commit should report Git checkpoint")
        .to_string();
    let heddle_after_commit = json(temp.path(), &["--output", "json", "status"])["current_state"]
        .as_str()
        .expect("status should report current Heddle state")
        .to_string();

    git(&["reset", "--soft", "HEAD~1"], temp.path());
    let current_git = git_stdout(temp.path(), &["rev-parse", "HEAD"]);
    assert_ne!(current_git, expected_git);

    let git_before_preview = git_ref_snapshot(temp.path());
    let preview = heddle_output(
        &["--output", "json", "undo", "--preview"],
        Some(temp.path()),
    )
    .expect("undo preview should run");
    assert!(!preview.status.success(), "unsafe preview should refuse");
    assert!(
        preview.stdout.is_empty(),
        "JSON-mode unsafe undo preview must keep stdout quiet: {}",
        String::from_utf8_lossy(&preview.stdout)
    );
    let preview_stderr = std::str::from_utf8(&preview.stderr).unwrap();
    let preview_envelope: Value =
        serde_json::from_str(preview_stderr).expect("unsafe preview should emit JSON envelope");
    assert_eq!(preview_envelope["kind"], "git_head_mismatch");
    assert_json_recovery_advice_fields(&preview_envelope, &preview_envelope.to_string());
    assert!(
        preview_envelope["unsafe_condition"]
            .as_str()
            .is_some_and(|condition| condition.contains(&current_git)
                && condition.contains(&expected_git)
                && condition.contains("dirty paths: modified: tracked.txt")),
        "unsafe preview should name current/expected Git OIDs and dirty paths: {preview_stderr}"
    );
    assert_eq!(
        preview_envelope["primary_command"],
        "heddle bridge git reconcile --prefer heddle --ref main --preview"
    );
    assert_eq!(
        git_ref_snapshot(temp.path()),
        git_before_preview,
        "unsafe preview refusal must not move Git refs"
    );
    assert_eq!(
        json(temp.path(), &["--output", "json", "status"])["current_state"],
        heddle_after_commit,
        "unsafe preview refusal must leave Heddle state untouched"
    );

    let output =
        heddle_output(&["--output", "json", "undo"], Some(temp.path())).expect("undo should run");
    assert!(!output.status.success(), "unsafe undo should refuse");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode unsafe undo must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: serde_json::Value =
        serde_json::from_str(stderr).expect("unsafe undo should emit JSON envelope");
    assert_eq!(envelope["kind"], preview_envelope["kind"]);
    assert_json_recovery_advice_fields(&envelope, &envelope.to_string());
    assert!(
        envelope["unsafe_condition"]
            .as_str()
            .is_some_and(|condition| condition.contains(&current_git)
                && condition.contains(&expected_git)
                && condition.contains("dirty paths: modified: tracked.txt")),
        "unsafe undo should name current/expected Git OIDs and reconcile preview: {stderr}"
    );
    assert_eq!(
        envelope["primary_command"], preview_envelope["primary_command"],
        "preview and real undo should share recovery advice"
    );

    assert_eq!(git_stdout(temp.path(), &["rev-parse", "HEAD"]), current_git);
    let status_after = json(temp.path(), &["--output", "json", "status"]);
    assert_eq!(
        status_after["current_state"], heddle_after_commit,
        "unsafe Git undo must leave Heddle state untouched: {status_after}"
    );
}

#[test]
fn git_overlay_matrix_bridge_push_pull_report_verification_state() {
    let temp = TempDir::new().unwrap();
    let origin = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(&["init", "--bare", "--initial-branch=main"], origin.path());
    std::fs::write(temp.path().join("tracked.txt"), "one\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(
        &["bridge", "git", "import", "--ref", "main"],
        Some(temp.path()),
    )
    .unwrap();

    std::fs::write(temp.path().join("tracked.txt"), "two\n").unwrap();
    json(temp.path(), &["--output", "json", "commit", "-m", "change"]);
    let origin_arg = origin.path().to_str().expect("origin path should be utf8");

    let push = json(
        temp.path(),
        &["--output", "json", "bridge", "git", "push", origin_arg],
    );
    assert_eq!(push["output_kind"], "bridge_git_push");
    assert_eq!(push["action"], "bridge git push");
    assert_eq!(push["status"], "pushed");
    assert_eq!(push["success"], true);
    assert_eq!(push["pushed"], true);
    assert_eq!(push["changed"], true);
    assert_eq!(push["transport"], "git");
    assert_eq!(push["remote"], origin_arg);
    assert_eq!(push["verification"]["verified"], true);
    assert_eq!(push["verification"]["status"], "clean");
    assert_verify_check_rows(&push["verification"]);

    let pull = json(
        temp.path(),
        &["--output", "json", "bridge", "git", "pull", origin_arg],
    );
    assert_eq!(pull["output_kind"], "bridge_git_pull");
    assert_eq!(pull["action"], "bridge git pull");
    assert_eq!(pull["status"], "up_to_date");
    assert_eq!(pull["success"], true);
    assert_eq!(pull["pulled"], false);
    assert_eq!(pull["changed"], false);
    assert_eq!(pull["transport"], "git");
    assert_eq!(pull["remote"], origin_arg);
    assert_eq!(pull["verification"]["verified"], true);
    assert_eq!(pull["verification"]["status"], "clean");
    assert_verify_check_rows(&pull["verification"]);
}

#[test]
fn git_overlay_matrix_top_level_push_closes_remote_verification_loop() {
    let temp = TempDir::new().unwrap();
    let origin = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(&["init", "--bare", "--initial-branch=main"], origin.path());
    std::fs::write(temp.path().join("tracked.txt"), "one\n").unwrap();
    git_commit_all(temp.path(), "seed");
    let origin_arg = origin.path().to_str().expect("origin path should be utf8");
    git(&["remote", "add", "origin", origin_arg], temp.path());
    git(&["push", "-u", "origin", "main"], temp.path());

    heddle(&["adopt"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "two\n").unwrap();
    let commit = json(temp.path(), &["--output", "json", "commit", "-m", "change"]);
    assert_eq!(commit["output_kind"], "commit");
    assert_eq!(commit["next_action"], "heddle push");
    assert_eq!(commit["next_action_template"]["argv_template"], heddle_argv_json(["push"]));
    assert_eq!(commit["recommended_action"], "heddle push");
    assert_eq!(
        commit["recommended_action_template"]["argv_template"],
        heddle_argv_json(["push"])
    );
    assert!(
        commit.get("next").is_none(),
        "old commit next alias removed"
    );
    assert!(
        commit.get("next_argv").is_none(),
        "old commit next_argv alias removed"
    );
    assert!(
        commit.get("next_template").is_none(),
        "old commit next_template alias removed"
    );
    let before_push = json(temp.path(), &["--output", "json", "verify"]);
    assert_eq!(before_push["verified"], true);
    assert_eq!(before_push["status"], "clean");
    assert_eq!(before_push["remote_drift"], "remote_ahead");
    assert_eq!(before_push["clone_verification"], "verified");
    assert_eq!(before_push["recommended_action"], "heddle push");
    assert!(
        before_push["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check["name"] == "Remote"
                && check["status"] == "remote_ahead"
                && check["clean"] == true
                && check["recommended_action"] == "heddle push"
                && check["details"]["ahead"] == "1"
                && check["details"]["behind"] == "0"),
        "local-ahead remote sync should be guidance, not a verify blocker: {before_push}"
    );
    let short_status = heddle(
        &["status", "--short", "--output", "text"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        short_status.contains("ready to push") && !short_status.contains("repository clean"),
        "short status should not claim a publish-ready checkout is merely clean: {short_status}"
    );

    let push = json(temp.path(), &["--output", "json", "push"]);
    assert_eq!(push["output_kind"], "push");
    assert_eq!(push["action"], "push");
    assert_eq!(push["success"], true);
    assert_eq!(push["pushed"], true);
    assert_eq!(push["transport"], "git");
    assert_eq!(push["push_scope"], "current_thread");
    assert_eq!(push["ref_scope"], "branch_and_heddle_notes");
    assert_eq!(push["git_notes_ref"], "refs/notes/heddle");
    assert!(
        push["git_notes_visibility_warning"]
            .as_str()
            .is_some_and(|warning| warning.contains("refs/notes/heddle")),
        "push should disclose the Git-visible Heddle notes ref: {push}"
    );
    assert_eq!(push["git_tracking_remote"], "origin");
    assert_eq!(push["git_remote_configured"], Value::Null);
    assert_eq!(push["git_upstream_configured"]["branch"], "main");
    assert_eq!(push["git_upstream_configured"]["remote"], "origin");
    assert_eq!(push["tags_included"], false);
    assert_eq!(push["next_action"], Value::Null);
    assert_eq!(push["next_action_argv"], Value::Null);
    assert_eq!(push["next_action_template"], Value::Null);
    assert_eq!(push["recommended_action"], Value::Null);
    assert_eq!(push["recommended_action_argv"], Value::Null);
    assert_eq!(push["recommended_action_template"], Value::Null);
    assert_eq!(push["verification"]["verified"], true);
    assert_eq!(push["verification"]["status"], "clean");

    let after_push = json(temp.path(), &["--output", "json", "verify"]);
    assert_eq!(after_push["verified"], true);
    assert_eq!(after_push["status"], "clean");
    assert_eq!(after_push["recommended_action"], Value::Null);
    assert_eq!(
        git_stdout(temp.path(), &["rev-parse", "refs/remotes/origin/main"]),
        git_stdout(temp.path(), &["rev-parse", "HEAD"])
    );
}

#[test]
fn git_overlay_matrix_commit_refuses_remote_divergence_before_capture() {
    let temp = TempDir::new().unwrap();
    let origin = TempDir::new().unwrap();
    let peer = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(&["init", "--bare", "--initial-branch=main"], origin.path());
    std::fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
    git_commit_all(temp.path(), "seed");
    let origin_arg = origin.path().to_str().expect("origin path should be utf8");
    git(&["remote", "add", "origin", origin_arg], temp.path());
    git(&["push", "-u", "origin", "main"], temp.path());
    heddle_adopt(temp.path());

    let peer_arg = peer.path().to_str().expect("peer path should be utf8");
    git(&["clone", origin_arg, peer_arg], temp.path());
    git(&["config", "user.name", "Peer"], peer.path());
    git(&["config", "user.email", "peer@example.com"], peer.path());

    std::fs::write(temp.path().join("tracked.txt"), "local\n").unwrap();
    json(
        temp.path(),
        &["--output", "json", "commit", "-m", "local checkpoint"],
    );
    let state_before = state_chain_ids(temp.path(), 8);
    let git_head_before = git_stdout(temp.path(), &["rev-parse", "HEAD"]);

    std::fs::write(peer.path().join("tracked.txt"), "remote\n").unwrap();
    git_commit_all(peer.path(), "remote checkpoint");
    git(&["push", "origin", "main"], peer.path());
    heddle(&["fetch", "origin"], Some(temp.path())).expect("fetch remote divergence");
    let verify = json(temp.path(), &["--output", "json", "verify"]);
    assert_eq!(verify["remote_drift"], "remote_diverged", "{verify}");

    std::fs::write(temp.path().join("extra.txt"), "blocked\n").unwrap();
    let output = heddle_output(
        &["--output", "json", "commit", "-m", "should not capture"],
        Some(temp.path()),
    )
    .expect("invoke commit against diverged upstream");
    assert!(
        !output.status.success(),
        "commit should refuse before capture when upstream has diverged"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON refusal should keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).expect("commit stderr utf8");
    let envelope: Value = serde_json::from_str(stderr).expect("commit refusal JSON parses");
    assert_eq!(envelope["kind"], "git_checkpoint_preflight_blocked");
    assert_eq!(
        envelope["primary_command"], "heddle bridge git import --ref origin/main",
        "{envelope}"
    );
    assert!(
        envelope["preserved"]
            .as_str()
            .is_some_and(|preserved| preserved.contains("Heddle refs")),
        "{envelope}"
    );
    assert_eq!(
        state_chain_ids(temp.path(), 8),
        state_before,
        "failed commit must not create a Heddle-only state"
    );
    assert_eq!(
        git_stdout(temp.path(), &["rev-parse", "HEAD"]),
        git_head_before,
        "failed commit must not move the local Git branch"
    );
    let status = json(temp.path(), &["--output", "json", "status"]);
    assert_ne!(
        status["verification"]["status"], "needs_checkpoint",
        "preflight refusal must not leave a captured-but-uncheckpointed state: {status}"
    );
}

#[test]
fn git_overlay_matrix_checkpoint_closes_imported_remote_divergence_after_merge() {
    let temp = TempDir::new().unwrap();
    let origin = TempDir::new().unwrap();
    let peer = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(&["init", "--bare", "--initial-branch=main"], origin.path());
    std::fs::write(temp.path().join("README.md"), "base\n").unwrap();
    git_commit_all(temp.path(), "seed");
    let origin_arg = origin.path().to_str().expect("origin path should be utf8");
    git(&["remote", "add", "origin", origin_arg], temp.path());
    git(&["push", "-u", "origin", "main"], temp.path());
    heddle_adopt(temp.path());

    let peer_arg = peer.path().to_str().expect("peer path should be utf8");
    git(&["clone", origin_arg, peer_arg], temp.path());
    git(&["config", "user.name", "Peer"], peer.path());
    git(&["config", "user.email", "peer@example.com"], peer.path());

    std::fs::write(temp.path().join("local.txt"), "local\n").unwrap();
    json(
        temp.path(),
        &["--output", "json", "commit", "-m", "local checkpoint"],
    );

    std::fs::write(peer.path().join("remote.txt"), "remote\n").unwrap();
    git_commit_all(peer.path(), "remote checkpoint");
    git(&["push", "origin", "main"], peer.path());
    heddle(&["fetch", "origin"], Some(temp.path())).expect("fetch remote divergence");

    let before_import = json(temp.path(), &["--output", "json", "verify"]);
    assert_eq!(
        before_import["remote_drift"], "remote_diverged",
        "{before_import}"
    );
    assert_eq!(
        before_import["recommended_action"], "heddle bridge git import --ref origin/main",
        "{before_import}"
    );

    let import = json(
        temp.path(),
        &[
            "--output",
            "json",
            "bridge",
            "git",
            "import",
            "--ref",
            "origin/main",
        ],
    );
    assert_eq!(import["branches_synced"], 1, "{import}");
    assert_eq!(import["states_created"], 1, "{import}");

    let preview = json(
        temp.path(),
        &["--output", "json", "merge", "origin/main", "--preview"],
    );
    assert_eq!(preview["preview_only"], true, "{preview}");
    assert_eq!(preview["conflict_count"], 0, "{preview}");

    let merged = json(temp.path(), &["--output", "json", "merge", "origin/main"]);
    assert_eq!(merged["status"], "completed", "{merged}");
    assert_eq!(merged["preview_only"], false, "{merged}");

    let needs_checkpoint = json(temp.path(), &["--output", "json", "status"]);
    assert_eq!(
        needs_checkpoint["verification"]["status"], "needs_checkpoint",
        "{needs_checkpoint}"
    );
    assert_eq!(
        needs_checkpoint["verification"]["remote_drift"], "remote_diverged",
        "{needs_checkpoint}"
    );
    assert_eq!(
        needs_checkpoint["recommended_action"], "heddle commit -m \"...\"",
        "after integrating upstream into Heddle, checkpoint must remain the primary way out: {needs_checkpoint}"
    );

    let checkpoint = json(
        temp.path(),
        &[
            "--output",
            "json",
            "checkpoint",
            "-m",
            "checkpoint integrated remote",
        ],
    );
    assert_eq!(checkpoint["status"], "checkpointed", "{checkpoint}");
    assert_eq!(
        checkpoint["verification"]["status"], "clean",
        "checkpoint should write a Git merge commit that can be pushed normally: {checkpoint}"
    );
    assert_eq!(checkpoint["verification"]["remote_drift"], "remote_ahead");
    assert_eq!(checkpoint["recommended_action"], "heddle push");
}

#[test]
fn git_overlay_matrix_imported_remote_divergence_surfaces_agree_on_next_action() {
    let temp = TempDir::new().unwrap();
    let origin = TempDir::new().unwrap();
    let peer = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(&["init", "--bare", "--initial-branch=main"], origin.path());
    std::fs::write(temp.path().join("README.md"), "base\n").unwrap();
    git_commit_all(temp.path(), "seed");
    let origin_arg = origin.path().to_str().expect("origin path should be utf8");
    git(&["remote", "add", "origin", origin_arg], temp.path());
    git(&["push", "-u", "origin", "main"], temp.path());
    heddle_adopt(temp.path());

    let peer_arg = peer.path().to_str().expect("peer path should be utf8");
    git(&["clone", origin_arg, peer_arg], temp.path());
    git(&["config", "user.name", "Peer"], peer.path());
    git(&["config", "user.email", "peer@example.com"], peer.path());

    std::fs::write(temp.path().join("local.txt"), "local\n").unwrap();
    json(
        temp.path(),
        &["--output", "json", "commit", "-m", "local checkpoint"],
    );

    std::fs::write(peer.path().join("remote.txt"), "remote\n").unwrap();
    git_commit_all(peer.path(), "remote checkpoint");
    git(&["push", "origin", "main"], peer.path());
    heddle(&["fetch", "origin"], Some(temp.path())).expect("fetch remote divergence");

    json(
        temp.path(),
        &[
            "--output",
            "json",
            "bridge",
            "git",
            "import",
            "--ref",
            "origin/main",
        ],
    );

    let merge_action = "heddle bridge git reconcile --ref origin/main --preview";
    let merge_argv = Some(heddle_argv_json([
        "bridge",
        "git",
        "reconcile",
        "--ref",
        "origin/main",
        "--preview",
    ]));
    for (label, output) in [
        ("status", json(temp.path(), &["--output", "json", "status"])),
        ("verify", json(temp.path(), &["--output", "json", "verify"])),
        ("doctor", json(temp.path(), &["--output", "json", "doctor"])),
        (
            "bridge git status",
            json(
                temp.path(),
                &["--output", "json", "bridge", "git", "status"],
            ),
        ),
    ] {
        assert_remote_divergence_surface(
            label,
            &output,
            "remote_diverged",
            "remote_diverged",
            merge_action,
            merge_argv.clone(),
        );
        assert_ne!(
            output["recommended_action"], "heddle land",
            "{label} must not recommend land for imported remote divergence: {output}"
        );
    }

    let status = json(temp.path(), &["--output", "json", "status"]);
    assert_eq!(
        status["remote_tracking"]["next_action"], merge_action,
        "status remote tracking action should not contradict the primary blocker: {status}"
    );
    let doctor = json(temp.path(), &["--output", "json", "doctor"]);
    assert_eq!(
        doctor["remote_tracking"]["next_action"], merge_action,
        "doctor remote tracking action should not contradict the primary blocker: {doctor}"
    );

    json(
        temp.path(),
        &["--output", "json", "merge", "origin/main", "--preview"],
    );
    json(temp.path(), &["--output", "json", "merge", "origin/main"]);

    let checkpoint_action = "heddle commit -m \"...\"";
    for (label, output) in [
        ("status", json(temp.path(), &["--output", "json", "status"])),
        ("verify", json(temp.path(), &["--output", "json", "verify"])),
        ("doctor", json(temp.path(), &["--output", "json", "doctor"])),
        (
            "bridge git status",
            json(
                temp.path(),
                &["--output", "json", "bridge", "git", "status"],
            ),
        ),
    ] {
        assert_remote_divergence_surface(
            label,
            &output,
            "needs_checkpoint",
            "remote_diverged",
            checkpoint_action,
            None,
        );
    }

    let checkpoint = json(
        temp.path(),
        &[
            "--output",
            "json",
            "checkpoint",
            "-m",
            "checkpoint integrated remote",
        ],
    );
    assert_eq!(checkpoint["verification"]["remote_drift"], "remote_ahead");
    for (label, output) in [
        ("status", json(temp.path(), &["--output", "json", "status"])),
        ("verify", json(temp.path(), &["--output", "json", "verify"])),
        ("doctor", json(temp.path(), &["--output", "json", "doctor"])),
        (
            "bridge git status",
            json(
                temp.path(),
                &["--output", "json", "bridge", "git", "status"],
            ),
        ),
    ] {
        assert_remote_divergence_surface(
            label,
            &output,
            "clean",
            "remote_ahead",
            "heddle push",
            Some(heddle_argv_json(["push"])),
        );
    }
}

#[test]
fn git_overlay_matrix_push_defaults_to_branch_upstream_remote() {
    let temp = TempDir::new().unwrap();
    let upstream = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(
        &["init", "--bare", "--initial-branch=main"],
        upstream.path(),
    );
    std::fs::write(temp.path().join("tracked.txt"), "one\n").unwrap();
    git_commit_all(temp.path(), "seed");
    let upstream_arg = upstream
        .path()
        .to_str()
        .expect("upstream path should be utf8");
    git(&["remote", "add", "upstream", upstream_arg], temp.path());
    git(&["push", "-u", "upstream", "main"], temp.path());

    heddle(&["adopt"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "two\n").unwrap();
    json(temp.path(), &["--output", "json", "commit", "-m", "change"]);
    let before_push = json(temp.path(), &["--output", "json", "verify"]);
    assert_eq!(before_push["verified"], true);
    assert_eq!(before_push["status"], "clean");
    assert_eq!(before_push["remote_drift"], "remote_ahead");
    assert_eq!(before_push["recommended_action"], "heddle push");

    let push = json(temp.path(), &["--output", "json", "push"]);
    assert_eq!(push["output_kind"], "push");
    assert_eq!(push["action"], "push");
    assert_eq!(push["pushed"], true);
    assert_eq!(push["remote"], "upstream");
    assert_eq!(push["git_tracking_remote"], "upstream");
    assert_eq!(push["git_upstream_configured"]["branch"], "main");
    assert_eq!(push["git_upstream_configured"]["remote"], "upstream");
    assert_eq!(push["verification"]["verified"], true);
    assert_eq!(push["verification"]["status"], "clean");
}

#[test]
fn git_overlay_matrix_local_only_branch_is_clean_until_push_sets_tracking() {
    let temp = TempDir::new().unwrap();
    let upstream = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(
        &["init", "--bare", "--initial-branch=main"],
        upstream.path(),
    );
    std::fs::write(temp.path().join("tracked.txt"), "one\n").unwrap();
    git_commit_all(temp.path(), "seed");
    let upstream_arg = upstream
        .path()
        .to_str()
        .expect("upstream path should be utf8");
    git(&["remote", "add", "upstream", upstream_arg], temp.path());
    git(&["push", "upstream", "main"], temp.path());

    heddle(&["adopt"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "two\n").unwrap();
    json(temp.path(), &["--output", "json", "commit", "-m", "change"]);
    let before_push = json(temp.path(), &["--output", "json", "verify"]);
    assert_eq!(before_push["verified"], true);
    assert_eq!(before_push["status"], "clean");
    assert_eq!(before_push["remote_drift"], "remote_untracked");
    assert_eq!(before_push["recommended_action"], "heddle push");
    assert_eq!(before_push["recovery_commands"], Value::Array(vec![]));

    let status_json = json(temp.path(), &["--output", "json", "status"]);
    assert_eq!(status_json["verification"]["verified"], true);
    assert_eq!(status_json["verification"]["status"], "clean");
    assert_eq!(
        status_json["verification"]["remote_drift"],
        "remote_untracked"
    );
    assert_ne!(status_json["coordination_status"], "blocked");
    assert_ne!(status_json["thread_state"], "blocked");

    let status_text = heddle(&["status", "--output", "text"], Some(temp.path())).unwrap();
    assert!(!status_text.contains("Remote drift:"), "{status_text}");
    assert!(
        !status_text.contains("Coordination: blocked"),
        "{status_text}"
    );

    let push = json(temp.path(), &["--output", "json", "push"]);
    assert_eq!(push["pushed"], true);
    assert_eq!(push["remote"], "upstream");
    assert_eq!(push["action"], "push");
    assert_eq!(push["git_tracking_remote"], "upstream");
    assert_eq!(push["git_upstream_configured"]["branch"], "main");
    assert_eq!(push["git_upstream_configured"]["remote"], "upstream");
    assert_eq!(push["verification"]["verified"], true);
    assert_eq!(push["verification"]["status"], "clean");
    assert_eq!(
        git_stdout(temp.path(), &["rev-parse", "--abbrev-ref", "@{upstream}"]),
        "upstream/main"
    );
}

#[test]
fn git_overlay_matrix_remote_add_configures_default_push_remote() {
    let temp = TempDir::new().unwrap();
    let audit = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(&["init", "--bare", "--initial-branch=main"], audit.path());
    std::fs::write(temp.path().join("tracked.txt"), "one\n").unwrap();
    git_commit_all(temp.path(), "seed");

    heddle(&["adopt"], Some(temp.path())).unwrap();
    let audit_arg = audit.path().to_str().expect("audit path should be utf8");
    let added = json(
        temp.path(),
        &["--output", "json", "remote", "add", "audit", audit_arg],
    );
    assert_eq!(added["output_kind"], "remote_add");
    assert_eq!(added["default"], "audit");
    assert_eq!(added["verification"]["default_remote"], "audit");
    assert_eq!(added["verification"]["verified"], true);
    assert_eq!(added["verification"]["status"], "clean");
    assert_eq!(added["verification"]["remote_drift"], "remote_untracked");
    assert_eq!(added["verification"]["recommended_action"], "heddle push");
    assert_eq!(
        git_stdout(temp.path(), &["remote", "get-url", "audit"]),
        audit_arg
    );

    let push = json(temp.path(), &["--output", "json", "push"]);
    assert_eq!(push["output_kind"], "push");
    assert_eq!(push["pushed"], true);
    assert_eq!(push["remote"], "audit");
    assert_eq!(push["verification"]["verified"], true);
    assert_eq!(push["verification"]["status"], "clean");

    let verify = json(temp.path(), &["--output", "json", "verify"]);
    assert_eq!(verify["verified"], true);
    assert_eq!(verify["status"], "clean");
    assert_eq!(verify["default_remote"], "audit");
    assert_eq!(verify["recommended_action"], Value::Null);
    assert_eq!(
        git_stdout(temp.path(), &["rev-parse", "refs/remotes/audit/main"]),
        git_stdout(temp.path(), &["rev-parse", "HEAD"])
    );
}

#[test]
fn git_overlay_matrix_remote_remove_clears_git_only_origin() {
    let temp = TempDir::new().unwrap();
    let origin = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(&["init", "--bare", "--initial-branch=main"], origin.path());
    std::fs::write(temp.path().join("tracked.txt"), "one\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle_adopt(temp.path());

    let origin_arg = origin.path().to_str().expect("origin path should be utf8");
    git(&["remote", "add", "origin", origin_arg], temp.path());

    let listed_before = json(temp.path(), &["--output", "json", "remote", "list"]);
    assert_eq!(
        listed_before["remotes"]
            .as_array()
            .expect("remotes array")
            .iter()
            .filter(|item| item["name"] == "origin")
            .count(),
        1,
        "Git-only origin should appear in heddle remote list: {listed_before}"
    );

    let removed = json(
        temp.path(),
        &["--output", "json", "remote", "remove", "origin"],
    );
    assert_eq!(removed["output_kind"], "remote_remove");
    assert_eq!(removed["status"], "completed");
    assert_eq!(removed["action"], "remote_remove");
    assert_eq!(removed["name"], "origin");

    let listed_after = json(temp.path(), &["--output", "json", "remote", "list"]);
    assert!(
        listed_after["remotes"]
            .as_array()
            .expect("remotes array")
            .iter()
            .all(|item| item["name"] != "origin"),
        "origin should be gone from heddle remote list after remove: {listed_after}"
    );

    let get_url = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(temp.path())
        .output()
        .expect("git remote get-url should run");
    assert!(
        !get_url.status.success(),
        "git remote get-url origin should fail after heddle remote remove: stdout={} stderr={}",
        String::from_utf8_lossy(&get_url.stdout),
        String::from_utf8_lossy(&get_url.stderr),
    );
}

#[test]
fn git_overlay_matrix_remote_remove_clears_both_sources() {
    let temp = TempDir::new().unwrap();
    let staging = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(&["init", "--bare", "--initial-branch=main"], staging.path());
    std::fs::write(temp.path().join("tracked.txt"), "one\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle_adopt(temp.path());

    let staging_arg = staging.path().to_str().expect("staging path should be utf8");
    let added = json(
        temp.path(),
        &["--output", "json", "remote", "add", "staging", staging_arg],
    );
    assert_eq!(added["output_kind"], "remote_add");
    assert_eq!(
        git_stdout(temp.path(), &["remote", "get-url", "staging"]),
        staging_arg,
        "remote add should populate Git-overlay .git/config"
    );

    let removed = json(
        temp.path(),
        &["--output", "json", "remote", "remove", "staging"],
    );
    assert_eq!(removed["output_kind"], "remote_remove");
    assert_eq!(removed["status"], "completed");
    assert_eq!(removed["name"], "staging");

    let listed = json(temp.path(), &["--output", "json", "remote", "list"]);
    assert!(
        listed["remotes"]
            .as_array()
            .expect("remotes array")
            .iter()
            .all(|item| item["name"] != "staging"),
        "staging should not reappear from .git/config after remove: {listed}"
    );

    let get_url = Command::new("git")
        .args(["remote", "get-url", "staging"])
        .current_dir(temp.path())
        .output()
        .expect("git remote get-url should run");
    assert!(
        !get_url.status.success(),
        "Git-overlay .git/config should also drop the staging remote",
    );
}

#[test]
fn git_overlay_matrix_remote_remove_unknown_returns_not_found() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "one\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle_adopt(temp.path());

    let output = heddle_output(
        &["--output", "json", "remote", "remove", "bogus"],
        Some(temp.path()),
    )
    .expect("invoke remote remove with unknown name");
    assert!(
        !output.status.success(),
        "remote remove on a missing name should fail"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value = serde_json::from_str(stderr)
        .unwrap_or_else(|err| panic!("remote remove failure should emit JSON: {err}: {stderr}"));
    assert_eq!(envelope["kind"], "remote_not_found");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("bogus")),
        "remote_not_found error should name the requested remote: {envelope}"
    );
}

#[test]
fn git_overlay_matrix_remote_set_default_accepts_git_only_remote() {
    let temp = TempDir::new().unwrap();
    let backup = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(&["init", "--bare", "--initial-branch=main"], backup.path());
    std::fs::write(temp.path().join("tracked.txt"), "one\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle_adopt(temp.path());

    let backup_arg = backup.path().to_str().expect("backup path should be utf8");
    git(&["remote", "add", "backup", backup_arg], temp.path());

    let set_default = json(
        temp.path(),
        &["--output", "json", "remote", "set-default", "backup"],
    );
    assert_eq!(set_default["output_kind"], "remote_set_default");
    assert_eq!(set_default["status"], "completed");
    assert_eq!(set_default["name"], "backup");
    assert_eq!(set_default["default"], "backup");

    let listed = json(temp.path(), &["--output", "json", "remote", "list"]);
    assert!(
        listed["remotes"]
            .as_array()
            .expect("remotes array")
            .iter()
            .any(|item| item["name"] == "backup" && item["is_default"] == true),
        "Git-only remote should be selectable as default: {listed}"
    );

    let verify = json(temp.path(), &["--output", "json", "verify"]);
    assert_eq!(
        verify["default_remote"], "backup",
        "verify should report the configured default remote: {verify}"
    );
}

#[test]
fn git_overlay_matrix_remote_set_default_works_for_dual_location_remote() {
    let temp = TempDir::new().unwrap();
    let origin = TempDir::new().unwrap();
    let staging = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(&["init", "--bare", "--initial-branch=main"], origin.path());
    git(&["init", "--bare", "--initial-branch=main"], staging.path());
    std::fs::write(temp.path().join("tracked.txt"), "one\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle_adopt(temp.path());

    let origin_arg = origin.path().to_str().expect("origin path should be utf8");
    let staging_arg = staging.path().to_str().expect("staging path should be utf8");
    json(
        temp.path(),
        &["--output", "json", "remote", "add", "origin", origin_arg],
    );
    json(
        temp.path(),
        &["--output", "json", "remote", "add", "staging", staging_arg],
    );

    let set_default = json(
        temp.path(),
        &["--output", "json", "remote", "set-default", "staging"],
    );
    assert_eq!(set_default["default"], "staging");

    let verify = json(temp.path(), &["--output", "json", "verify"]);
    assert_eq!(verify["default_remote"], "staging");
}

#[test]
fn git_overlay_matrix_remote_set_default_unknown_returns_not_found() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "one\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle_adopt(temp.path());

    let output = heddle_output(
        &["--output", "json", "remote", "set-default", "bogus"],
        Some(temp.path()),
    )
    .expect("invoke remote set-default with unknown name");
    assert!(
        !output.status.success(),
        "remote set-default on a missing name should fail"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value = serde_json::from_str(stderr).unwrap_or_else(|err| {
        panic!("remote set-default failure should emit JSON: {err}: {stderr}")
    });
    assert_eq!(envelope["kind"], "remote_not_found");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("bogus")),
        "remote_not_found error should name the requested remote: {envelope}"
    );
}

#[test]
fn git_overlay_matrix_local_ahead_noop_merge_preserves_semantic_result() {
    let temp = TempDir::new().unwrap();
    let origin = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(&["init", "--bare", "--initial-branch=main"], origin.path());
    std::fs::write(temp.path().join("tracked.txt"), "one\n").unwrap();
    git_commit_all(temp.path(), "seed");
    let origin_arg = origin.path().to_str().expect("origin path should be utf8");
    git(&["remote", "add", "origin", origin_arg], temp.path());
    git(&["push", "-u", "origin", "main"], temp.path());

    heddle(&["adopt"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "two\n").unwrap();
    json(temp.path(), &["--output", "json", "commit", "-m", "change"]);

    let merge = json(
        temp.path(),
        &["--output", "json", "merge", "main", "--preview"],
    );
    assert_eq!(merge["status"], "completed");
    assert_eq!(merge["semantic_result"], "already_up_to_date");
    let verify = json(temp.path(), &["--output", "json", "verify"]);
    assert_eq!(verify["verified"], true);
    assert_eq!(verify["status"], "clean");
    assert_eq!(verify["remote_drift"], "remote_ahead");
    assert_eq!(verify["recommended_action"], "heddle push");
}

#[test]
fn git_overlay_matrix_subdirectory_dirty_commands() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle_adopt(temp.path());

    let nested = temp.path().join("src/deep/nested");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "tracked modified").unwrap();
    std::fs::write(temp.path().join("new.txt"), "new").unwrap();

    let status = json(&nested, &["status", "--output", "json"]);
    assert_eq!(status["thread"], "feature/drop-in");
    assert!(
        status["changes"]["modified"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "tracked.txt")
    );
    assert!(
        status["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "new.txt")
    );

    let diagnose = json(&nested, &["doctor", "--output", "json"]);
    assert_eq!(diagnose["changes"]["total"], 2);

    let show = json(&nested, &["show", "HEAD", "--output", "json"]);
    assert!(show["change_id"].as_str().is_some());

    let log = json(&nested, &["log", "--output", "json"]);
    assert!(
        !log["states"].as_array().unwrap().is_empty(),
        "log should resolve from nested repo paths: {log}"
    );

    let diff = json(&nested, &["diff", "HEAD"]);
    assert!(
        diff["changes"]["modified"].as_array().is_some()
            && diff["changes"]["added"].as_array().is_some()
            && diff["changes"]["deleted"].as_array().is_some(),
        "diff should remain well-formed (category object) after nested-path bootstrap/show/log sequencing: {diff}"
    );

    let thread_list = json(&nested, &["thread", "list", "--output", "json"]);
    assert_eq!(thread_list["current"], "feature/drop-in");

    let workspace = json(&nested, &["workspace", "show", "--output", "json"]);
    assert_eq!(workspace["current_thread"], "feature/drop-in");
}

#[test]
fn git_overlay_matrix_manual_git_commit_after_bootstrap_commands() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    heddle_adopt(temp.path());

    std::fs::write(temp.path().join("tracked.txt"), "tracked committed via git").unwrap();
    git(&["add", "tracked.txt"], temp.path());
    git(&["commit", "-m", "manual git commit"], temp.path());

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["thread"], "feature/drop-in");
    assert_eq!(status["verification"]["status"], "git_branch_advanced");
    assert_eq!(
        status["git_overlay_health"]["status"],
        "git_branch_advanced"
    );
    assert_eq!(
        status["verification"]["mapping_state"],
        "git_branch_advanced"
    );
    assert_eq!(status["verification"]["import_state"], "needs_import");
    assert!(
        status["verification"]["summary"]
            .as_str()
            .is_some_and(|summary| {
                summary.contains("Git branch 'feature/drop-in' advanced outside Heddle")
            }),
        "status JSON should identify external Git branch advancement: {status}"
    );
    assert_eq!(
        status["changed_path_count"], 0,
        "a clean Git worktree with an unimported Git commit should not look like unsaved Heddle work: {status}"
    );
    assert!(
        status["changes"]["modified"].as_array().unwrap().is_empty(),
        "branch-tip drift should not be reported as unsaved modified paths: {status}"
    );
    assert_eq!(
        status["recommended_action"],
        "heddle adopt --ref feature/drop-in"
    );
    assert_eq!(
        status["recommended_action_template"]["argv_template"],
        heddle_argv_json(["adopt", "--ref", "feature/drop-in"])
    );
    assert_eq!(
        status["verification"]["recommended_action"],
        "heddle adopt --ref feature/drop-in"
    );
    assert_eq!(status["verification"]["workflow_status"], "not_checked");
    assert_eq!(status["verification"]["worktree_state"], "not_checked");
    let status_text = heddle(&["status", "--output", "text", "-v"], Some(temp.path())).unwrap();
    assert!(
        status_text.contains(
            "Verification: Git branch 'feature/drop-in' advanced outside Heddle; import the new Git tip to restore the mapping"
        )
            && status_text.contains("Health: Git branch advanced outside Heddle")
            && status_text.contains("heddle adopt --ref feature/drop-in")
            && !status_text.contains("Setup needed: Git repo detected")
            && !status_text.contains("Git worktree: clean; .heddle metadata is present")
            && !status_text.contains("Changes not yet saved")
            && status_text.contains(
                "No unsaved worktree changes detected; import the external Git branch tip before comparing Heddle state"
            ),
        "text status should clearly distinguish external Git branch advancement from first-run setup or unsaved work: {status_text}"
    );

    let verify = json(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(verify["status"], "git_branch_advanced");
    assert_eq!(verify["mapping_state"], "git_branch_advanced");
    assert_eq!(
        verify["recommended_action"],
        "heddle adopt --ref feature/drop-in"
    );
    assert!(
        verify["summary"].as_str().is_some_and(|summary| {
            summary.contains("Git branch 'feature/drop-in' advanced outside Heddle")
        }),
        "verify JSON should identify external Git branch advancement: {verify}"
    );
    let verify_text_output = heddle_output(&["verify", "--output", "text"], Some(temp.path()))
        .expect("invoke strict verify text");
    assert!(
        !verify_text_output.status.success(),
        "blocked verify text should exit nonzero"
    );
    let verify_text = String::from_utf8_lossy(&verify_text_output.stdout);
    assert!(
        verify_text.contains("Git branch 'feature/drop-in' advanced outside Heddle")
            && verify_text.contains("heddle adopt --ref feature/drop-in")
            && !verify_text.contains("Setup needed: Git repo detected"),
        "verify text should identify external Git branch advancement, not first-run setup: {verify_text}"
    );

    let bridge = json(
        temp.path(),
        &["bridge", "git", "status", "--output", "json"],
    );
    assert_eq!(
        bridge["git_overlay_health"]["status"],
        "git_branch_advanced"
    );
    assert_eq!(bridge["verification"]["status"], "git_branch_advanced");
    assert_eq!(
        bridge["recommended_action"],
        "heddle adopt --ref feature/drop-in"
    );
    let bridge_text = heddle(
        &["bridge", "git", "status", "--output", "text"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        bridge_text.contains("Git branch 'feature/drop-in' advanced outside Heddle")
            && bridge_text.contains("Git branch waiting for Heddle import: feature/drop-in")
            && bridge_text.contains("Recovery: heddle adopt --ref feature/drop-in")
            && !bridge_text.contains("Optional Git-only branch available: feature/drop-in")
            && !bridge_text.contains("Setup needed"),
        "bridge git status text should identify mapping drift and exact recovery: {bridge_text}"
    );

    let show = json(temp.path(), &["show", "HEAD", "--output", "json"]);
    assert!(show["change_id"].as_str().is_some());

    let log = json(temp.path(), &["log", "--output", "json"]);
    assert!(
        !log["states"].as_array().unwrap().is_empty(),
        "log should still succeed after plain git commits: {log}"
    );

    let same_state_diff = json(temp.path(), &["diff", "HEAD", "HEAD"]);
    assert_eq!(same_state_diff["stats"]["files_changed"], 0);

    let diagnose = json(temp.path(), &["doctor", "--output", "json"]);
    assert_eq!(
        diagnose["changes"]["total"], 0,
        "diagnose must not resurrect stale Heddle-vs-state paths when Git is clean but import is needed: {diagnose}"
    );
    let diff = json(temp.path(), &["diff", "--output", "json", "--stat"]);
    let diff_changes = diff["changes"]
        .as_object()
        .unwrap_or_else(|| panic!("worktree diff changes should be a category object: {diff}"));
    assert!(
        ["modified", "added", "deleted"]
            .iter()
            .all(|key| diff_changes[*key].as_array().is_some_and(|a| a.is_empty())),
        "diff must not report stale paths when Git is clean but import is needed: {diff}"
    );

    std::fs::write(temp.path().join("tracked.txt"), "dirty after manual git\n").unwrap();
    let dirty_status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(dirty_status["changed_path_count"], 1);
    assert_eq!(dirty_status["verification"]["worktree_state"], "dirty");
    let dirty_diagnose = json(temp.path(), &["doctor", "--output", "json"]);
    assert_eq!(
        dirty_diagnose["changes"]["total"], 1,
        "diagnose should show the same current Git dirty set as status under needs_import: {dirty_diagnose}"
    );
    let dirty_diff = json(temp.path(), &["diff", "--output", "json", "--stat"]);
    let dirty_changes = dirty_diff["changes"]
        .as_object()
        .unwrap_or_else(|| panic!("worktree diff changes should be a category object: {dirty_diff}"));
    let dirty_total: usize = ["modified", "added", "deleted"]
        .iter()
        .filter_map(|key| dirty_changes[*key].as_array())
        .map(Vec::len)
        .sum();
    assert_eq!(
        dirty_total, 1,
        "diff should show the same current Git dirty set as status under needs_import: {dirty_diff}"
    );

    let ready = json(
        temp.path(),
        &["--output", "json", "ready", "-m", "carry branch work"],
    );
    assert_eq!(ready["status"], "blocked");
    assert_eq!(
        ready["recommended_action"],
        "heddle adopt --ref feature/drop-in"
    );
}

#[test]
fn git_overlay_matrix_raw_git_reset_reports_reconcile_not_unsaved_work() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle_adopt(temp.path());

    std::fs::write(temp.path().join("tracked.txt"), "heddle change\n").unwrap();
    let committed = json(
        temp.path(),
        &["--output", "json", "commit", "-m", "heddle change"],
    );
    let heddle_state = committed["change_id"]
        .as_str()
        .expect("commit should report Heddle state")
        .to_string();

    git(&["reset", "--hard", "HEAD~1"], temp.path());
    assert_eq!(git_status_short(temp.path()), "");
    let reset_head = git_stdout(temp.path(), &["rev-parse", "HEAD"]);

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["verification"]["status"], "needs_reconcile");
    assert_eq!(status["git_overlay_health"]["status"], "needs_reconcile");
    assert_eq!(status["verification"]["mapping_state"], "needs_reconcile");
    assert_eq!(status["changed_path_count"], 0);
    assert!(status["changes"]["modified"].as_array().unwrap().is_empty());
    assert!(status["changes"]["added"].as_array().unwrap().is_empty());
    assert!(status["changes"]["deleted"].as_array().unwrap().is_empty());
    assert_eq!(
        status["recommended_action"],
        "heddle bridge git reconcile --ref main --preview"
    );
    assert_eq!(
        status["recommended_action_template"]["argv_template"],
        heddle_argv_json(["bridge", "git", "reconcile", "--ref", "main", "--preview"])
    );
    assert!(
        status["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|blocker| !blocker.as_str().unwrap_or_default().contains("Clone:")),
        "status blockers should name the mapping disagreement, not clone verification fallout: {status}"
    );
    assert!(
        status["verification"]["summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("Git branch 'main'")
                && summary.contains("Heddle thread state")),
        "status should describe Git/Heddle disagreement: {status}"
    );
    let status_text = heddle(&["status", "--output", "text", "-v"], Some(temp.path())).unwrap();
    assert!(
        status_text.contains("Git/Heddle mismatch")
            && status_text.contains("Health: Git/Heddle mismatch")
            && status_text.contains("Git branch 'main'")
            && !status_text.contains("Health: needs_reconcile")
            && !status_text.contains("clone verification is blocked"),
        "human status should make raw reset a mapping/reconcile problem: {status_text}"
    );

    let verify = json(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(verify["status"], "needs_reconcile");
    assert_eq!(
        verify["recommended_action"],
        "heddle bridge git reconcile --ref main --preview"
    );

    let bridge = json(
        temp.path(),
        &["bridge", "git", "status", "--output", "json"],
    );
    assert_eq!(bridge["verification"]["status"], "needs_reconcile");
    assert_eq!(
        bridge["recommended_action"],
        "heddle bridge git reconcile --ref main --preview"
    );

    let refused = heddle_output(
        &[
            "--output",
            "json",
            "commit",
            "-m",
            "follow bad reset advice",
        ],
        Some(temp.path()),
    )
    .expect("commit should run");
    assert!(
        !refused.status.success(),
        "commit should refuse reconcile drift"
    );
    assert!(refused.stdout.is_empty());
    let stderr = std::str::from_utf8(&refused.stderr).unwrap();
    let envelope: Value = serde_json::from_str(stderr).expect("reconcile refusal should be JSON");
    assert_eq!(envelope["kind"], "repository_verification_blocked");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("Refusing to commit")),
        "commit should refuse as commit, not leak capture wording: {envelope}"
    );
    assert_eq!(
        envelope["primary_command"],
        "heddle bridge git reconcile --ref main --preview"
    );
    assert_eq!(
        git_stdout(temp.path(), &["rev-parse", "HEAD"]),
        reset_head,
        "refused commit must not recreate the reset-away Git commit"
    );
    assert_eq!(
        json(temp.path(), &["status", "--output", "json"])["current_state"],
        heddle_state,
        "refused commit must not add a new Heddle state"
    );
}

#[test]
fn git_overlay_matrix_branch_lifecycle_refreshes_import_hints() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    // Import-hint information has moved to `heddle bridge git status
    // --output json`; per-command outputs (status, log, show, workspace,
    // thread list) no longer carry it.
    git(&["branch", "support/original"], temp.path());
    let bridge_before = json(
        temp.path(),
        &["bridge", "git", "status", "--output", "json"],
    );
    assert_eq!(
        bridge_before["git_overlay_import_hint"]["missing_branch_count"],
        2
    );
    assert!(
        bridge_before["git_overlay_import_hint"]["missing_branches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|branch| branch == "feature/drop-in"),
        "plain Git active branch should stay visible as unimported before adoption: {bridge_before}"
    );
    assert_eq!(
        bridge_before["git_overlay_import_hint"]["missing_branches"][1],
        "support/original"
    );

    git(
        &["branch", "-m", "support/original", "support/renamed"],
        temp.path(),
    );
    let bridge_after_rename = json(
        temp.path(),
        &["bridge", "git", "status", "--output", "json"],
    );
    assert_eq!(
        bridge_after_rename["git_overlay_import_hint"]["missing_branches"][1],
        "support/renamed"
    );

    git(&["branch", "-D", "support/renamed"], temp.path());
    let bridge_after_delete = json(
        temp.path(),
        &["bridge", "git", "status", "--output", "json"],
    );
    assert_eq!(
        bridge_after_delete["git_overlay_import_hint"]["missing_branches"],
        serde_json::json!(["feature/drop-in"]),
        "deleting the extra branch should leave only the active plain-Git branch to adopt: {bridge_after_delete}"
    );

    git(&["branch", "support/recreated"], temp.path());
    let bridge_after_recreate = json(
        temp.path(),
        &["bridge", "git", "status", "--output", "json"],
    );
    assert_eq!(
        bridge_after_recreate["git_overlay_import_hint"]["missing_branches"][1],
        "support/recreated"
    );
}

#[test]
fn git_overlay_matrix_branch_delete_does_not_recommend_deleted_thread() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle_adopt(temp.path());

    json(
        temp.path(),
        &[
            "--output",
            "json",
            "start",
            "feature/delete-text",
            "--workspace",
            "materialized",
        ],
    );
    let text = heddle(&["branch", "-d", "feature/delete-text"], Some(temp.path())).unwrap();
    assert!(
        text.contains("Dropped thread 'feature/delete-text'"),
        "branch -d should report the dropped thread: {text}"
    );
    assert!(
        !text.contains("heddle ready --thread feature/delete-text") && !text.contains("Next:"),
        "branch -d must not point at the deleted thread: {text}"
    );

    json(
        temp.path(),
        &[
            "--output",
            "json",
            "start",
            "feature/delete-json",
            "--workspace",
            "materialized",
        ],
    );
    let deleted = json(
        temp.path(),
        &["--output", "json", "branch", "-d", "feature/delete-json"],
    );
    assert_eq!(deleted["status"], "completed");
    assert_eq!(deleted["thread"]["state"], "abandoned");
    assert_eq!(
        deleted["next_action"],
        Value::Null,
        "deleted thread output must not carry a dead next action: {deleted}"
    );
    assert_eq!(deleted["recommended_action"], Value::Null);
}

#[test]
fn git_overlay_matrix_auto_adopts_local_branch_tips_without_full_import() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/alpha"], temp.path());
    git(&["branch", "support/beta"], temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();

    let thread_list = json(temp.path(), &["thread", "list", "--output", "json"]);
    let threads = thread_list["threads"].as_array().unwrap();
    assert!(
        threads
            .iter()
            .all(|thread| thread["name"] != "support/alpha"),
        "available Git branch tips should not be modeled as active threads: {thread_list}"
    );
    let available_refs = thread_list["available_git_refs"].as_array().unwrap();
    let alpha = available_refs
        .iter()
        .find(|git_ref| git_ref["name"] == "support/alpha")
        .expect("support/alpha should appear as an available Git ref");
    assert!(
        alpha["git_commit"]
            .as_str()
            .is_some_and(|oid| !oid.is_empty())
    );
    assert_eq!(
        alpha["recommended_action"],
        "heddle adopt --ref support/alpha"
    );

    let beta_show = json(
        temp.path(),
        &["thread", "show", "support/beta", "--output", "json"],
    );
    assert_eq!(beta_show["name"], "support/beta");
    assert_eq!(beta_show["history_imported"], false);
    assert!(beta_show["git_branch_tip"].as_str().is_some());

    let workspace = json(temp.path(), &["workspace", "show", "--output", "json"]);
    let workspace_threads = workspace["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| group["threads"].as_array().into_iter().flatten())
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        workspace_threads
            .iter()
            .all(|thread| thread["name"] != "support/alpha"),
        "workspace groups should only contain active threads: {workspace}"
    );
    assert!(
        workspace["available_git_refs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|git_ref| git_ref["name"] == "support/alpha")
    );

    // Import-hint information has moved to `heddle bridge git status
    // --output json`; per-command outputs no longer carry it.
    let bridge = json(
        temp.path(),
        &["bridge", "git", "status", "--output", "json"],
    );
    assert_eq!(bridge["git_overlay_import_hint"]["missing_branch_count"], 3);
}

#[test]
fn git_overlay_matrix_import_marks_branch_tip_history_as_imported() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/imported"], temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();

    let before = json(
        temp.path(),
        &["thread", "show", "support/imported", "--output", "json"],
    );
    assert_eq!(before["history_imported"], false);

    heddle(&["bridge", "import", "--path", "."], Some(temp.path())).unwrap();

    let after = json(
        temp.path(),
        &["thread", "show", "support/imported", "--output", "json"],
    );
    assert_eq!(after["history_imported"], true);
    assert!(after["git_branch_tip"].as_str().is_some());
}

#[test]
fn git_overlay_matrix_non_main_default_branch_commands() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "develop");
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("feature.txt"), "feature work").unwrap();

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["thread"], "develop");

    let diagnose = json(temp.path(), &["doctor", "--output", "json"]);
    assert_eq!(diagnose["thread"]["name"], "develop");

    let thread_list = json(temp.path(), &["thread", "list", "--output", "json"]);
    assert_eq!(thread_list["current"], "develop");

    let workspace = json(temp.path(), &["workspace", "show", "--output", "json"]);
    assert_eq!(workspace["current_thread"], "develop");
}

#[test]
fn git_overlay_matrix_detached_head_sequence_commands() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(
        &["bridge", "git", "import", "--ref", "feature/drop-in"],
        Some(temp.path()),
    )
    .unwrap();

    git(&["checkout", "--detach", "HEAD"], temp.path());
    std::fs::write(temp.path().join("detached.txt"), "detached work").unwrap();

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert!(
        status["thread"].is_null(),
        "detached Git HEAD should not be reported as the last attached branch: {status}"
    );
    assert_eq!(status["git_overlay_health"]["status"], "detached_head");
    assert_eq!(status["verification"]["status"], "detached_head");
    assert!(status["verification"]["git_branch"].is_null());
    assert!(status["verification"]["heddle_thread"].is_null());
    assert!(
        status["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "detached.txt")
    );

    let verify = json(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(verify["status"], "detached_head");
    assert!(verify["git_branch"].is_null());
    assert!(verify["heddle_thread"].is_null());
    assert_eq!(
        verify["recommended_action"], "heddle switch feature/drop-in",
        "detached-head recovery should stay inside Heddle's no-git runtime: {verify}"
    );
    assert_eq!(
        verify["recommended_action_template"]["argv_template"],
        heddle_argv_json(["switch", "feature/drop-in"])
    );
    assert!(
        verify["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check["name"] == "Mapping" && check["status"] == "detached_head"),
        "verify should surface the detached Git HEAD mapping blocker: {verify}"
    );

    let show = json(temp.path(), &["show", "HEAD", "--output", "json"]);
    assert!(show["change_id"].as_str().is_some());

    let log = json(temp.path(), &["log", "--output", "json"]);
    assert!(
        !log["states"].as_array().unwrap().is_empty(),
        "detached HEAD should still have a visible history surface: {log}"
    );
}

#[test]
fn git_overlay_matrix_commit_refuses_detached_head_without_advancing_branch() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(
        &["bridge", "git", "import", "--ref", "main"],
        Some(temp.path()),
    )
    .unwrap();

    let before_head = git_stdout(temp.path(), &["rev-parse", "HEAD"]);
    let before_main = git_stdout(temp.path(), &["rev-parse", "refs/heads/main"]);
    git(&["checkout", "--detach", "HEAD"], temp.path());
    std::fs::write(temp.path().join("detached-commit.txt"), "detached work").unwrap();

    let output = heddle_output(
        &["--output", "json", "commit", "-m", "detached commit"],
        Some(temp.path()),
    )
    .expect("heddle commit should run");
    assert!(
        !output.status.success(),
        "commit must refuse on detached Git HEAD: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("Git HEAD is detached"),
        "detached-head refusal should be explicit: {combined}"
    );
    let envelope: Value =
        serde_json::from_str(&combined).expect("detached-head refusal should be a JSON envelope");
    assert_eq!(envelope["primary_command"], "heddle switch main");
    assert_eq!(
        envelope["primary_command_template"]["argv_template"],
        heddle_argv_json(["switch", "main"])
    );
    assert!(
        !combined.contains("git switch"),
        "detached-head refusal should not depend on the Git CLI: {combined}"
    );

    let symbolic = Command::new("git")
        .args(["symbolic-ref", "-q", "HEAD"])
        .current_dir(temp.path())
        .output()
        .expect("git symbolic-ref should run");
    assert!(
        !symbolic.status.success(),
        "failed commit must leave Git HEAD detached"
    );
    assert_eq!(git_stdout(temp.path(), &["rev-parse", "HEAD"]), before_head);
    assert_eq!(
        git_stdout(temp.path(), &["rev-parse", "refs/heads/main"]),
        before_main,
        "failed detached-head commit must not advance or reattach main"
    );
}

#[test]
fn git_overlay_matrix_detached_at_tag_status_commands() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["tag", "v1.0.0"], temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    git(&["checkout", "v1.0.0"], temp.path());
    std::fs::write(temp.path().join("detached-tag.txt"), "detached tag work").unwrap();

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_git_overlay_basics(&status);
    assert!(status["thread"].is_null());
    assert_eq!(status["git_overlay_health"]["status"], "detached_head");
    assert_eq!(status["verification"]["status"], "detached_head");
    assert!(
        status["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "detached-tag.txt"),
        "status should remain usable when detached at a tag: {status}"
    );

    let diagnose = json(temp.path(), &["doctor", "--output", "json"]);
    assert_git_overlay_basics(&diagnose);

    let show = json(temp.path(), &["show", "HEAD", "--output", "json"]);
    assert_git_overlay_basics(&show);
    assert!(show["change_id"].as_str().is_some());
}

#[test]
fn git_overlay_matrix_dirty_branch_switch_when_git_allows_carryover() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("shared.txt"), "base").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/carry"], temp.path());
    heddle_adopt(temp.path());

    std::fs::write(temp.path().join("shared.txt"), "carried modification").unwrap();
    git(&["checkout", "support/carry"], temp.path());
    std::fs::write(temp.path().join("carry.txt"), "branch local").unwrap();

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["thread"], "support/carry");
    assert!(
        status["changes"]["modified"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "shared.txt")
    );
    assert!(
        status["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "carry.txt")
    );

    let ready = json(
        temp.path(),
        &["--output", "json", "ready", "-m", "first-run ready state"],
    );
    assert_eq!(ready["captured"], true);

    let after_ready = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(after_ready["thread"], "support/carry");
    assert!(
        after_ready["changes"]["modified"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        after_ready["changes"]["added"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn git_overlay_matrix_no_commit_first_run_durability_commands() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "trunk");
    std::fs::write(temp.path().join("checkpoint.txt"), "first run").unwrap();

    let same_state_diff = json(temp.path(), &["diff", "HEAD", "HEAD"]);
    assert_eq!(same_state_diff["stats"]["files_changed"], 0);

    let ready = json(temp.path(), &["--output", "json", "ready"]);
    assert_eq!(ready["thread_state"], "active");

    let checkpoint = json(temp.path(), &["checkpoint", "-m", "First-run checkpoint"]);
    assert_eq!(checkpoint["summary"], "First-run checkpoint");
    assert_eq!(checkpoint["storage_model"], "git+heddle-sidecar");
    assert!(checkpoint["git_commit"].as_str().is_some());

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert!(status["git_checkpoint"]["git_commit"].as_str().is_some());
}

#[test]
fn git_overlay_matrix_imported_branch_evolution_after_bridge_import() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    git(&["branch", "support/alpha"], temp.path());
    git(&["branch", "support/beta"], temp.path());

    let before = json(
        temp.path(),
        &["bridge", "git", "status", "--output", "json"],
    );
    assert_eq!(before["git_overlay_import_hint"]["missing_branch_count"], 3);
    assert!(
        before["git_overlay_import_hint"]["missing_branches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|branch| branch == "feature/drop-in"),
        "plain Git active branch should be counted until bridge import runs: {before}"
    );

    let import_output = heddle(&["bridge", "import", "--path", "."], Some(temp.path())).unwrap();
    assert!(
        import_output.contains("branches") || import_output.contains("\"branches_synced\""),
        "bridge import should report branch sync activity: {import_output}"
    );

    let after_import = json(temp.path(), &["thread", "list", "--output", "json"]);
    assert!(
        after_import["threads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|thread| thread["name"] == "support/alpha")
    );
    assert!(
        after_import["threads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|thread| thread["name"] == "support/beta")
    );

    git(
        &["branch", "-m", "support/alpha", "support/alpha-renamed"],
        temp.path(),
    );
    git(&["branch", "-D", "support/beta"], temp.path());
    git(&["branch", "support/gamma"], temp.path());

    let status = json(
        temp.path(),
        &["bridge", "git", "status", "--output", "json"],
    );
    assert_eq!(
        status["git_overlay_import_hint"],
        Value::Null,
        "renamed or newly-created branches at already imported commits should not reopen import work: {status}"
    );
    let thread_list = json(temp.path(), &["thread", "list", "--output", "json"]);
    let available = thread_list["available_git_refs"]
        .as_array()
        .expect("thread list should expose available Git refs");
    assert!(
        available
            .iter()
            .any(|git_ref| git_ref["name"] == "support/alpha-renamed"),
        "renamed imported branch should be a calm optional Git-only branch: {thread_list}"
    );
    assert!(
        available
            .iter()
            .any(|git_ref| git_ref["name"] == "support/gamma"),
        "new Git branch at an imported commit should be a calm optional Git-only branch: {thread_list}"
    );
    assert!(
        available
            .iter()
            .all(|git_ref| git_ref["name"] != "support/beta"),
        "deleted Git branch should not remain visible as an optional branch: {thread_list}"
    );
}

#[test]
fn git_overlay_matrix_stale_conflict_ship_blocks_with_guidance() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("conflict.txt"), "base\n").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle_adopt(temp.path());

    let started = json(
        temp.path(),
        &[
            "--output",
            "json",
            "start",
            "feature/conflict",
            "--workspace",
            "materialized",
        ],
    );
    let thread_path = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());

    std::fs::write(thread_path.join("conflict.txt"), "thread change\n").unwrap();
    heddle(&["capture", "-m", "thread change"], Some(&thread_path)).unwrap();

    std::fs::write(temp.path().join("conflict.txt"), "main change\n").unwrap();
    json(
        temp.path(),
        &["--output", "json", "commit", "-m", "main change"],
    );

    let before_ship = json(
        temp.path(),
        &["thread", "show", "feature/conflict", "--output", "json"],
    );
    assert_eq!(before_ship["freshness"], "stale");

    let ship_output = heddle_output(
        &["--output", "json", "land", "--thread", "feature/conflict"],
        Some(temp.path()),
    )
    .expect("invoke blocked conflict land");
    assert!(
        !ship_output.status.success(),
        "blocked conflict land should exit nonzero"
    );
    let land: Value = serde_json::from_slice(&ship_output.stdout)
        .unwrap_or_else(|err| panic!("blocked conflict land should emit JSON on stdout: {err}"));
    assert_eq!(land["status"], "blocked");
    assert_eq!(land["checkpointed"], false);
    assert_eq!(land["integrated"], false);
    assert!(
        land["next_action"]
            .as_str()
            .unwrap_or("")
            .contains("sync --thread feature/conflict"),
        "blocked land should surface the next operator step: {land}"
    );

    let thread_show = json(
        temp.path(),
        &["thread", "show", "feature/conflict", "--output", "json"],
    );
    assert_eq!(thread_show["thread_state"], "active");
    assert!(
        thread_show["recommended_action"]
            .as_str()
            .unwrap_or("")
            .contains("sync --thread feature/conflict")
    );
}

#[test]
fn git_overlay_matrix_conflicted_merge_exits_nonzero_after_writing_markers() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("conflict.txt"), "base\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle_adopt(temp.path());

    heddle(
        &["thread", "create", "feature/conflict-merge"],
        Some(temp.path()),
    )
    .unwrap();
    heddle(
        &["thread", "switch", "feature/conflict-merge"],
        Some(temp.path()),
    )
    .unwrap();
    std::fs::write(temp.path().join("conflict.txt"), "feature\n").unwrap();
    heddle(&["capture", "-m", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("conflict.txt"), "main\n").unwrap();
    heddle(&["capture", "-m", "main"], Some(temp.path())).unwrap();

    let merge = heddle_output(
        &["--output", "json", "merge", "feature/conflict-merge"],
        Some(temp.path()),
    )
    .expect("invoke conflicted merge");
    assert!(
        !merge.status.success(),
        "conflicted mutating merge should exit nonzero"
    );
    let parsed: Value = serde_json::from_slice(&merge.stdout)
        .unwrap_or_else(|err| panic!("conflicted merge should emit JSON on stdout: {err}"));
    assert_eq!(parsed["status"], "blocked", "{parsed}");
    assert_eq!(parsed["conflict_count"], 1, "{parsed}");
    assert_eq!(parsed["conflicts"], serde_json::json!(["conflict.txt"]));
    assert!(
        parsed["recommended_action"]
            .as_str()
            .is_some_and(|action| action == "heddle sync --thread feature/conflict-merge"),
        "stale conflicted merge should refresh before materializing conflict markers: {parsed}"
    );
    let conflict_file = std::fs::read_to_string(temp.path().join("conflict.txt")).unwrap();
    assert!(
        !conflict_file.contains("<<<<<<<") && conflict_file == "main\n",
        "stale merge refusal must not materialize conflict markers"
    );
}

#[test]
fn git_overlay_matrix_stale_conflict_thread_resolve_enters_conflict_recovery() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("conflict.txt"), "base\n").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle_adopt(temp.path());

    let started = json(
        temp.path(),
        &[
            "--output",
            "json",
            "start",
            "feature/resolve-conflict",
            "--workspace",
            "materialized",
        ],
    );
    let thread_path = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());

    std::fs::write(thread_path.join("conflict.txt"), "thread change\n").unwrap();
    heddle(&["capture", "-m", "thread change"], Some(&thread_path)).unwrap();

    std::fs::write(temp.path().join("conflict.txt"), "main change\n").unwrap();
    json(
        temp.path(),
        &["--output", "json", "commit", "-m", "main change"],
    );

    let preview_output = heddle_output(
        &[
            "--output",
            "json",
            "merge",
            "feature/resolve-conflict",
            "--preview",
        ],
        Some(temp.path()),
    )
    .expect("invoke stale conflict merge preview");
    assert!(
        !preview_output.status.success(),
        "stale conflict preview should exit nonzero"
    );
    assert!(
        preview_output.stdout.is_empty(),
        "strict JSON preview refusal should emit the envelope on stderr"
    );
    let preview_stderr = std::str::from_utf8(&preview_output.stderr).unwrap();
    let preview: Value = serde_json::from_str(preview_stderr)
        .unwrap_or_else(|err| panic!("expected JSON stderr: {err}: {preview_stderr}"));
    assert_eq!(preview["kind"], "merge_preview_blocked", "{preview}");
    assert_eq!(
        preview["primary_command"],
        "heddle sync --thread feature/resolve-conflict"
    );
    assert_eq!(preview["conflict_count"], 1, "{preview}");
    assert_eq!(preview["conflicts"], serde_json::json!(["conflict.txt"]));
    assert_eq!(preview["semantic_result"], "path_conflicts", "{preview}");

    let resolved = json(
        temp.path(),
        &[
            "--output",
            "json",
            "thread",
            "resolve",
            "feature/resolve-conflict",
        ],
    );
    assert_eq!(resolved["status"], "blocked", "{resolved}");
    assert!(
        resolved["next_action"]
            .as_str()
            .is_some_and(|action| action.contains("conflict list")),
        "thread resolve should point at materialized conflict state: {resolved}"
    );
    assert!(
        resolved["recommended_action"]
            .as_str()
            .is_some_and(|action| action.contains("resolve conflict.txt")),
        "thread resolve should make the next file resolution executable: {resolved}"
    );
    assert!(
        !resolved["recommended_action"]
            .as_str()
            .unwrap_or("")
            .contains("thread refresh"),
        "thread resolve must not loop back to refresh after writing conflict state: {resolved}"
    );
    let conflict_file = std::fs::read_to_string(thread_path.join("conflict.txt")).unwrap();
    assert!(
        conflict_file.contains("<<<<<<<"),
        "thread resolve should materialize conflict markers in the isolated checkout"
    );
}

#[test]
fn git_overlay_matrix_reopen_from_different_cwds_preserves_state_and_git_only_aliases() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle_adopt(temp.path());
    git(&["branch", "support/reopen-me"], temp.path());

    let root_status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(root_status["thread"], "feature/drop-in");
    let root_bridge = json(
        temp.path(),
        &["bridge", "git", "status", "--output", "json"],
    );
    assert_eq!(
        root_bridge["git_overlay_import_hint"],
        Value::Null,
        "a branch alias at an already adopted commit should not reopen import work: {root_bridge}"
    );
    let root_threads = json(temp.path(), &["thread", "list", "--output", "json"]);
    assert!(
        root_threads["available_git_refs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|git_ref| git_ref["name"] == "support/reopen-me"),
        "branch alias should stay visible as optional work from the repo root: {root_threads}"
    );

    let nested = temp.path().join("src/reopen/check");
    std::fs::create_dir_all(&nested).unwrap();
    let nested_workspace = json(&nested, &["workspace", "show", "--output", "json"]);
    assert_eq!(nested_workspace["current_thread"], "feature/drop-in");
    let nested_bridge = json(&nested, &["bridge", "git", "status", "--output", "json"]);
    assert_eq!(
        nested_bridge["git_overlay_import_hint"],
        Value::Null,
        "nested bridge status should agree that branch alias history is already imported: {nested_bridge}"
    );

    std::fs::write(temp.path().join("tracked.txt"), "tracked after reopen").unwrap();
    let ready = json(
        &nested,
        &["--output", "json", "ready", "-m", "nested ready capture"],
    );
    assert_eq!(ready["captured"], true);

    let root_show = json(temp.path(), &["show", "HEAD", "--output", "json"]);
    assert!(root_show["change_id"].as_str().is_some());

    let nested_log = json(&nested, &["log", "--output", "json"]);
    assert!(
        !nested_log["states"].as_array().unwrap().is_empty(),
        "reopened nested cwd should still see persisted history: {nested_log}"
    );

    let root_status_after = json(temp.path(), &["status", "--output", "json"]);
    assert!(
        root_status_after["changes"]["modified"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let root_bridge_after = json(
        temp.path(),
        &["bridge", "git", "status", "--output", "json"],
    );
    assert_eq!(
        root_bridge_after["git_overlay_import_hint"],
        Value::Null,
        "captured but uncheckpointed work should ask for a checkpoint, not reopen import work: {root_bridge_after}"
    );
    assert_eq!(
        root_bridge_after["verification"]["status"],
        "needs_checkpoint"
    );
}

#[test]
fn git_overlay_matrix_binary_file_commands_remain_coherent() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("binary.bin"), vec![0u8, 1, 2, 3, 255]).unwrap();
    git_commit_all(temp.path(), "seed binary");
    heddle_adopt(temp.path());

    std::fs::write(temp.path().join("binary.bin"), vec![9u8, 8, 7, 6, 5]).unwrap();

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert!(
        status["changes"]["modified"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "binary.bin")
    );

    let diff_output = heddle(&["diff", "HEAD"], Some(temp.path())).unwrap();
    assert!(
        diff_output.contains("binary.bin") || diff_output.contains("\"path\":\"binary.bin\""),
        "binary diff should stay coherent and mention the changed file: {diff_output}"
    );

    let ready = json(
        temp.path(),
        &["--output", "json", "ready", "-m", "binary ready capture"],
    );
    assert_eq!(ready["captured"], true);

    let status_after = json(temp.path(), &["status", "--output", "json"]);
    assert!(
        status_after["changes"]["modified"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[cfg(unix)]
#[test]
fn git_overlay_matrix_clean_dangling_symlink_does_not_look_committable() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    symlink("missing-target.txt", temp.path().join("dangling.txt")).unwrap();
    git_commit_all(temp.path(), "seed dangling symlink");
    assert_eq!(git_status_short(temp.path()), "");

    heddle_adopt(temp.path());

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(
        status["changed_path_count"], 0,
        "Git-clean dangling symlink should not create Heddle worktree changes: {status}"
    );
    assert_eq!(
        status["changes"]["modified"],
        serde_json::json!([]),
        "Git-clean dangling symlink should not be modified: {status}"
    );
    assert_eq!(
        status["git_index"]["unstaged_paths"],
        serde_json::json!([]),
        "Git index plan must compare symlinks by link target, not Path::exists(): {status}"
    );
    assert_eq!(
        status["git_index"]["will_commit"],
        serde_json::json!([]),
        "plain commit must not claim it would include a Git-clean symlink: {status}"
    );
}

/// Git-overlay status path (`render_worktree_status_diff`, the path cid
/// 3321103601 flagged): a tracked regular file removed and a symlink (whose
/// followed bytes match the removed file's) added scores as a rename
/// candidate but crosses the regular↔symlink boundary. The `--patch`/JSON
/// render captures each side's mode and so keeps it split; the status renders
/// (default / `--stat` / `--name-only`) used to drop modes before rename
/// detection and silently re-collapse it into a rename. Pin every status
/// render on this overlay path to "split, not rename".
#[cfg(unix)]
#[test]
fn git_overlay_matrix_diff_status_keeps_cross_type_move_split() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("mover.txt"), "shared payload\n").unwrap();
    std::fs::write(temp.path().join("anchor.txt"), "shared payload\n").unwrap();
    std::fs::write(temp.path().join("filler.txt"), "filler\n").unwrap();
    git_commit_all(temp.path(), "seed mover + anchor");
    heddle_adopt(temp.path());

    // Advance the Git branch outside Heddle (an unrelated commit) so the repo
    // enters `git_branch_advanced` — that drift state is what makes
    // `trust_visible_worktree_status` trust the live worktree and route
    // `heddle diff` through `render_worktree_status_diff` (the path cid
    // 3321103601 flagged), instead of the heddle-native builder.
    std::fs::write(temp.path().join("filler.txt"), "filler edit\n").unwrap();
    git(&["add", "filler.txt"], temp.path());
    git(&["commit", "-m", "advance branch outside heddle"], temp.path());

    // The cross-type move stays UNCOMMITTED in the worktree: `linked` follows
    // to `anchor.txt`, so the worktree blob read for the added side equals the
    // removed `mover.txt` bytes — a similarity-1.0 rename candidate that must
    // still stay split across the regular↔symlink boundary.
    std::fs::remove_file(temp.path().join("mover.txt")).unwrap();
    symlink("anchor.txt", temp.path().join("linked")).unwrap();

    // Sanity: confirm we are actually on the `render_worktree_status_diff`
    // path — the drift state must be `git_branch_advanced`.
    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(
        status["verification"]["status"], "git_branch_advanced",
        "test must exercise the trusted-worktree status path: {status}"
    );

    let default = heddle(&["diff"], Some(temp.path())).unwrap();
    assert!(
        !default.contains("rename from"),
        "overlay default render must keep the cross-type move split:\n{default}"
    );
    let stat = heddle(&["diff", "--stat"], Some(temp.path())).unwrap();
    assert!(
        !stat.contains("renamed") && !stat.contains(" -> "),
        "overlay --stat must keep the cross-type move split:\n{stat}"
    );
    let name_only = heddle(&["diff", "--name-only"], Some(temp.path())).unwrap();
    assert!(
        name_only.lines().any(|line| line == "mover.txt")
            && name_only.lines().any(|line| line == "linked"),
        "overlay --name-only must list both `mover.txt` (deleted) and `linked` \
         (added), not collapse to one renamed path:\n{name_only}"
    );
}

#[cfg(unix)]
#[test]
fn git_overlay_matrix_diff_added_symlink_renders_link_target() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    git_commit_all(temp.path(), "seed readme");
    heddle_adopt(temp.path());

    symlink("README.md", temp.path().join("link-to-readme")).unwrap();
    let diff = json(temp.path(), &["--output", "json", "diff"]);
    let link_change = diff["changes"]["added"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "link-to-readme")
        .unwrap_or_else(|| panic!("diff should include added symlink under the added category: {diff}"));
    assert_eq!(link_change["kind"], "added");
    let added_line = link_change["lines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|line| line["prefix"] == "+" && line["content"] == "README.md");
    assert!(
        added_line.is_some(),
        "symlink diff must show the link target, not the target file contents: {diff}"
    );
    assert!(
        !link_change["lines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|line| line["content"] == "hello"),
        "symlink diff must not dereference the link target: {diff}"
    );
}

#[cfg(unix)]
#[test]
fn git_overlay_matrix_symlink_status_and_ready_work() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("target.txt"), "target").unwrap();
    symlink("target.txt", temp.path().join("link.txt")).unwrap();
    git_commit_all(temp.path(), "seed symlink");
    heddle_adopt(temp.path());

    std::fs::remove_file(temp.path().join("link.txt")).unwrap();
    symlink("other.txt", temp.path().join("link.txt")).unwrap();

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert!(
        status["changes"]["modified"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "link.txt")
    );

    let ready = json(
        temp.path(),
        &["--output", "json", "ready", "-m", "symlink ready capture"],
    );
    assert_eq!(ready["captured"], true);

    let show = json(temp.path(), &["show", "HEAD", "--output", "json"]);
    assert!(show["change_id"].as_str().is_some());
}

#[cfg(unix)]
#[test]
fn git_overlay_matrix_filemode_changes_surface_and_capture() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("script.sh"), "#!/bin/sh\necho hi\n").unwrap();
    git_commit_all(temp.path(), "seed script");
    heddle_adopt(temp.path());

    let script = temp.path().join("script.sh");
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert!(
        status["changes"]["modified"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "script.sh")
    );

    let ready = json(
        temp.path(),
        &["--output", "json", "ready", "-m", "mode ready capture"],
    );
    assert_eq!(ready["captured"], true);

    let checkpoint = json(temp.path(), &["checkpoint", "-m", "mode checkpoint"]);
    assert!(checkpoint["git_commit"].as_str().is_some());
}

#[test]
fn git_overlay_matrix_stale_thread_can_recover_via_sync_then_ship() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle_adopt(temp.path());

    let started = json(
        temp.path(),
        &[
            "--output",
            "json",
            "start",
            "feature/recover",
            "--workspace",
            "materialized",
        ],
    );
    let thread_path = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());

    std::fs::write(thread_path.join("feature.txt"), "feature work").unwrap();
    heddle(&["capture", "-m", "feature work"], Some(&thread_path)).unwrap();

    std::fs::write(temp.path().join("base.txt"), "base updated").unwrap();
    json(
        temp.path(),
        &["--output", "json", "commit", "-m", "advance main"],
    );

    let before_sync = json(
        temp.path(),
        &["thread", "show", "feature/recover", "--output", "json"],
    );
    assert_eq!(before_sync["freshness"], "stale");

    let sync = json(
        temp.path(),
        &["--output", "json", "sync", "--thread", "feature/recover"],
    );
    assert_eq!(sync["status"], "refreshed");
    assert_eq!(sync["chosen_path"], "refresh");

    let after_sync = json(
        temp.path(),
        &["thread", "show", "feature/recover", "--output", "json"],
    );
    assert_eq!(after_sync["freshness"], "current");

    let land = json(
        temp.path(),
        &["--output", "json", "land", "--thread", "feature/recover"],
    );
    assert_eq!(land["status"], "landed");
    assert_eq!(land["checkpointed"], true);
    assert!(land["git_commit"].as_str().is_some());
    assert_eq!(
        land["performed_steps"],
        serde_json::json!(["merge", "checkpoint"])
    );
    assert_eq!(
        land["skipped_steps"],
        serde_json::json!([
            "capture(no changes)",
            "sync(current)",
            "push(not requested)"
        ])
    );
}

#[test]
fn git_overlay_matrix_stale_merge_preview_blocks_ship_recommendation_and_diff() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle_adopt(temp.path());

    let started = json(
        temp.path(),
        &[
            "--output",
            "json",
            "start",
            "feature/stale-preview",
            "--workspace",
            "materialized",
        ],
    );
    let thread_path = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());

    std::fs::write(thread_path.join("feature.txt"), "feature work\n").unwrap();
    let ready = json(
        &thread_path,
        &["--output", "json", "ready", "-m", "feature ready"],
    );
    assert_eq!(ready["status"], "completed");

    std::fs::write(
        temp.path().join("base.txt"),
        "base\nadvanced outside heddle\n",
    )
    .unwrap();
    git_commit_all(temp.path(), "external main advance");
    heddle(&["adopt", "--ref", "main"], Some(temp.path())).unwrap();

    let thread_show = json(
        temp.path(),
        &[
            "thread",
            "show",
            "feature/stale-preview",
            "--output",
            "json",
        ],
    );
    assert_eq!(thread_show["freshness"], "stale");

    let parent_status = json(temp.path(), &["--output", "json", "status"]);
    assert_eq!(
        parent_status["recommended_action"], "heddle sync --thread feature/stale-preview",
        "parent status must refresh a stale ready thread before merge preview is actionable: {parent_status}"
    );
    assert_ne!(
        parent_status["recommended_action"],
        "heddle merge feature/stale-preview --preview",
        "parent status must not recommend merge preview for a stale ready thread: {parent_status}"
    );

    let no_diff_preview_output = heddle_output(
        &[
            "--output",
            "json",
            "merge",
            "feature/stale-preview",
            "--preview",
        ],
        Some(temp.path()),
    )
    .expect("invoke stale merge preview without diff");
    assert!(
        !no_diff_preview_output.status.success(),
        "stale merge preview without --with-diff must fail closed too"
    );
    assert!(
        no_diff_preview_output.stdout.is_empty(),
        "strict JSON refusal should emit one JSON document on stderr only"
    );
    let stderr = std::str::from_utf8(&no_diff_preview_output.stderr).unwrap();
    let no_diff_preview: Value = serde_json::from_str(stderr)
        .unwrap_or_else(|err| panic!("expected JSON stderr: {err}: {stderr}"));
    assert_eq!(no_diff_preview["kind"], "merge_preview_blocked");
    assert_eq!(
        no_diff_preview["primary_command"],
        "heddle sync --thread feature/stale-preview"
    );

    let preview_output = heddle_output(
        &[
            "--output",
            "json",
            "merge",
            "feature/stale-preview",
            "--preview",
            "--with-diff",
        ],
        Some(temp.path()),
    )
    .expect("invoke stale merge preview");
    assert!(
        !preview_output.status.success(),
        "stale merge preview that did not run must be a strict failure"
    );
    assert!(
        preview_output.stdout.is_empty(),
        "strict JSON refusal should emit one JSON document on stderr only"
    );
    let stderr = std::str::from_utf8(&preview_output.stderr).unwrap();
    let preview: Value = serde_json::from_str(stderr)
        .unwrap_or_else(|err| panic!("expected JSON stderr: {err}: {stderr}"));
    assert_json_recovery_advice_fields(&preview, "stale merge preview refusal");
    assert_eq!(preview["kind"], "merge_preview_blocked");
    assert_eq!(
        preview["primary_command"],
        "heddle sync --thread feature/stale-preview"
    );
    assert_eq!(
        preview["recovery_commands"],
        serde_json::json!(["heddle sync --thread feature/stale-preview"])
    );
    assert!(
        preview["unsafe_condition"]
            .as_str()
            .unwrap_or("")
            .contains("stale"),
        "blocked preview should still identify stale sync state: {preview}"
    );
    assert!(
        !preview["primary_command"]
            .as_str()
            .unwrap_or("")
            .contains("land"),
        "stale preview must not recommend land: {preview}"
    );
}

#[test]
fn git_overlay_matrix_verify_blocked_merge_preview_exits_nonzero() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle_adopt(temp.path());

    let started = json(
        temp.path(),
        &[
            "--output",
            "json",
            "start",
            "feature/verify-blocked-preview",
            "--workspace",
            "materialized",
        ],
    );
    let thread_path = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());
    std::fs::write(thread_path.join("feature.txt"), "feature work\n").unwrap();
    let ready = json(
        &thread_path,
        &["--output", "json", "ready", "-m", "feature ready"],
    );
    assert_eq!(ready["status"], "completed");

    std::fs::write(temp.path().join(".git").join("MERGE_HEAD"), "deadbeef\n").unwrap();
    let output = heddle_output(
        &[
            "--output",
            "json",
            "merge",
            "feature/verify-blocked-preview",
            "--preview",
        ],
        Some(temp.path()),
    )
    .expect("invoke verification-blocked merge preview");
    assert!(
        !output.status.success(),
        "merge preview must fail strictly when verification prevents the preview from running"
    );
    assert!(output.stdout.is_empty());
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value = serde_json::from_str(stderr)
        .unwrap_or_else(|err| panic!("expected JSON stderr: {err}: {stderr}"));
    assert_json_recovery_advice_fields(&envelope, "verification-blocked merge preview refusal");
    assert_eq!(envelope["kind"], "merge_preview_blocked");
    assert!(
        envelope["unsafe_condition"]
            .as_str()
            .unwrap_or("")
            .contains("Operation: Git merge is in progress"),
        "verification-blocked preview should name the blocking check: {envelope}"
    );
}

#[test]
fn git_overlay_matrix_ship_uses_thread_intent_for_git_checkpoint_subject() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("README.md"), "base\n").unwrap();
    git_commit_all(temp.path(), "base");
    heddle_adopt(temp.path());

    let feature_path = temp.path().with_extension("feature-land-message");
    json(
        temp.path(),
        &[
            "--output",
            "json",
            "start",
            "feature/land-message",
            "--path",
            feature_path.to_str().unwrap(),
        ],
    );
    std::fs::write(feature_path.join("tags.txt"), "tags\n").unwrap();
    let ready = json(
        &feature_path,
        &["--output", "json", "ready", "-m", "Add evaluation tags"],
    );
    assert_eq!(ready["status"], "completed");

    let land = json(
        temp.path(),
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/land-message",
            "--no-push",
        ],
    );
    assert_eq!(land["status"], "landed");
    assert_eq!(land["checkpointed"], true);
    assert!(land["git_commit"].as_str().is_some());
    assert_eq!(
        git_stdout(temp.path(), &["log", "-1", "--pretty=%s"]),
        "Add evaluation tags"
    );

    let checkpoint_records_path = temp.path().join(".heddle/state/git-checkpoints.json");
    let checkpoint_records: Value =
        serde_json::from_str(&std::fs::read_to_string(checkpoint_records_path).unwrap()).unwrap();
    assert!(
        checkpoint_records
            .as_array()
            .unwrap()
            .iter()
            .any(|record| record["summary"] == "Add evaluation tags"),
        "land should record the meaningful thread intent as the Git checkpoint summary: {checkpoint_records}"
    );
}

#[test]
fn git_overlay_matrix_ship_undo_restores_git_and_heddle_together() {
    let temp = TempDir::new().unwrap();
    let origin = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(&["init", "--bare", "--initial-branch=main"], origin.path());
    std::fs::write(temp.path().join("README.md"), "base\n").unwrap();
    git_commit_all(temp.path(), "base");
    let origin_arg = origin.path().to_str().expect("origin path should be utf8");
    git(&["remote", "add", "origin", origin_arg], temp.path());
    git(&["push", "-u", "origin", "main"], temp.path());
    heddle_adopt(temp.path());

    let feature_path = temp.path().with_extension("feature-land-undo");
    let started = json(
        temp.path(),
        &[
            "--output",
            "json",
            "start",
            "feature/land-undo",
            "--path",
            feature_path.to_str().unwrap(),
        ],
    );
    assert_eq!(started["thread"]["name"], "feature/land-undo");

    std::fs::write(feature_path.join("README.md"), "base\nfeature\n").unwrap();
    std::fs::write(feature_path.join("feature.txt"), "feature\n").unwrap();
    let ready = json(
        &feature_path,
        &[
            "--output",
            "json",
            "ready",
            "-m",
            "feature ready for land undo",
        ],
    );
    assert_eq!(ready["status"], "completed");

    let base_verify = json(temp.path(), &["--output", "json", "verify"]);
    assert_eq!(base_verify["verified"], true);
    assert_eq!(base_verify["status"], "clean");
    assert_eq!(
        base_verify["recommended_action"],
        "heddle land --thread feature/land-undo --no-push"
    );

    let land = json(
        temp.path(),
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/land-undo",
            "--no-push",
        ],
    );
    assert_eq!(land["status"], "landed");
    assert_eq!(land["checkpointed"], true);
    assert_eq!(land["verification"]["verified"], true);
    assert_eq!(land["verification"]["status"], "clean");
    assert_eq!(land["verification"]["recommended_action"], "heddle push");
    assert_eq!(land["recommended_action"], "heddle push");
    assert_eq!(land["next_action"], "heddle push");
    assert!(land["git_commit"].as_str().is_some());

    let after_ship = json(temp.path(), &["--output", "json", "verify"]);
    assert_eq!(after_ship["verified"], true);
    assert_eq!(after_ship["status"], "clean");
    assert_eq!(after_ship["recommended_action"], "heddle push");
    let thread_after_ship = json(
        temp.path(),
        &["--output", "json", "thread", "show", "feature/land-undo"],
    );
    assert_eq!(thread_after_ship["thread_state"], "merged");
    assert_eq!(
        thread_after_ship["integration_policy_result"]["status"],
        "auto_integrated"
    );

    let undo = json(temp.path(), &["--output", "json", "undo"]);
    assert_eq!(undo["action"], "undo");

    let after_undo = json(temp.path(), &["--output", "json", "verify"]);
    assert_eq!(
        after_undo["verified"], true,
        "land undo should not leave Git/Heddle mapping blocked: {after_undo}"
    );
    assert_eq!(after_undo["status"], "clean");
    assert_eq!(
        after_undo["recommended_action"],
        "heddle land --thread feature/land-undo --no-push"
    );
    let status_after_undo = json(temp.path(), &["--output", "json", "status"]);
    assert_eq!(
        status_after_undo["recommended_action"], "heddle land --thread feature/land-undo --no-push",
        "status should restore the ready workflow action after land undo: {status_after_undo}"
    );
    let thread_list_after_undo = json(temp.path(), &["--output", "json", "thread", "list"]);
    assert_eq!(
        thread_list_after_undo["recommended_action"], "heddle land --thread feature/land-undo --no-push",
        "thread list should restore the ready workflow action after land undo: {thread_list_after_undo}"
    );
    let workspace_after_undo = json(temp.path(), &["--output", "json", "workspace", "show"]);
    assert_eq!(
        workspace_after_undo["recommended_action"], "heddle land --thread feature/land-undo --no-push",
        "workspace should restore the ready workflow action after land undo: {workspace_after_undo}"
    );
    assert_eq!(git_status_short(temp.path()), "");
    assert!(
        !temp.path().join("feature.txt").exists(),
        "undo should remove the feature file from the main Git worktree"
    );

    let thread_after_undo = json(
        temp.path(),
        &["--output", "json", "thread", "show", "feature/land-undo"],
    );
    assert_eq!(
        thread_after_undo["thread_state"], "ready",
        "undoing the land should unmark the source thread as merged: {thread_after_undo}"
    );
    assert_eq!(
        thread_after_undo["integration_policy_result"]["status"],
        Value::Null,
        "undoing the land should clear stale auto-integrated metadata: {thread_after_undo}"
    );

    let thread_record_path = std::fs::read_dir(temp.path().join(".heddle/thread_records"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| {
            std::fs::read_to_string(path)
                .map(|content| content.contains(r#"thread = "feature/land-undo""#))
                .unwrap_or(false)
        })
        .expect("feature thread record should exist");
    let thread_record = std::fs::read_to_string(&thread_record_path).unwrap();
    let stale_record = thread_record.replace(r#"state = "ready""#, r#"state = "merged""#);
    std::fs::write(&thread_record_path, stale_record).unwrap();
    let stale_verify = json(temp.path(), &["--output", "json", "verify"]);
    assert_eq!(stale_verify["status"], "stale_integration_metadata");
    let workflow = stale_verify["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "Workflow")
        .unwrap_or_else(|| panic!("verify should include Workflow check: {stale_verify}"));
    assert_eq!(workflow["status"], "stale_integration_metadata");

    std::fs::write(temp.path().join("local-dirty.txt"), "local dirty\n").unwrap();
    let redo_refusal =
        heddle_output(&["--output", "json", "redo"], Some(temp.path())).expect("redo should run");
    assert!(!redo_refusal.status.success(), "dirty redo should refuse");
    let stderr = String::from_utf8_lossy(&redo_refusal.stderr);
    let envelope: Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|err| panic!("dirty redo should emit JSON envelope: {err}: {stderr}"));
    assert_eq!(envelope["code"], "dirty_worktree");
    assert!(
        !temp.path().join("feature.txt").exists(),
        "redo refusal must not partially re-apply Heddle/worktree state"
    );
    let dirty_verify = json(temp.path(), &["--output", "json", "verify"]);
    assert_eq!(
        dirty_verify["status"], "dirty_worktree",
        "dirty redo refusal should leave only the user's dirty path, not a needs-checkpoint partial apply: {dirty_verify}"
    );

    std::fs::remove_file(temp.path().join("local-dirty.txt")).unwrap();
    let redo = json(temp.path(), &["--output", "json", "redo"]);
    assert_eq!(redo["action"], "redo");
    assert_eq!(redo["status"], "completed");
    assert_eq!(redo["verification"]["verified"], true);
    assert_eq!(redo["verification"]["status"], "clean");
    assert_eq!(git_status_short(temp.path()), "");
    assert!(
        temp.path().join("feature.txt").exists(),
        "redo should restore the landed feature file once preflights pass"
    );

    let thread_after_redo = json(
        temp.path(),
        &["--output", "json", "thread", "show", "feature/land-undo"],
    );
    assert_eq!(thread_after_redo["thread_state"], "merged");
    assert_eq!(
        thread_after_redo["integration_policy_result"]["status"],
        "auto_integrated"
    );
}

#[test]
fn git_overlay_matrix_ship_push_without_remote_refuses_before_mutation() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("README.md"), "base\n").unwrap();
    git_commit_all(temp.path(), "base");
    heddle_adopt(temp.path());

    let feature_path = temp.path().with_extension("feature-no-remote-push");
    json(
        temp.path(),
        &[
            "--output",
            "json",
            "start",
            "feature/no-remote-push",
            "--path",
            feature_path.to_str().unwrap(),
        ],
    );
    std::fs::write(feature_path.join("README.md"), "base\nfeature\n").unwrap();
    let ready = json(
        &feature_path,
        &[
            "--output",
            "json",
            "ready",
            "-m",
            "feature ready without remote",
        ],
    );
    assert_eq!(ready["status"], "completed");

    let preview = json(
        temp.path(),
        &[
            "--output",
            "json",
            "merge",
            "feature/no-remote-push",
            "--preview",
        ],
    );
    assert_eq!(
        preview["recommended_action"],
        "heddle land --thread feature/no-remote-push --no-push"
    );
    assert_eq!(
        preview["recommended_action_template"]["argv_template"],
        heddle_argv_json(["land", "--thread", "feature/no-remote-push", "--no-push"])
    );
    assert_eq!(
        preview["verification"]["recommended_action"],
        "heddle land --thread feature/no-remote-push --no-push"
    );

    let before_state = json(temp.path(), &["--output", "json", "status"])["current_state"].clone();
    let before_git = git_stdout(temp.path(), &["rev-parse", "HEAD"]);
    let before_refs = git_ref_snapshot(temp.path());
    let push = heddle_output(
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/no-remote-push",
            "--push",
        ],
        Some(temp.path()),
    )
    .expect("invoke land --push");
    assert!(
        !push.status.success(),
        "land --push should refuse without a remote"
    );
    assert!(
        push.stdout.is_empty(),
        "JSON refusal should not emit partial stdout: {}",
        String::from_utf8_lossy(&push.stdout)
    );
    let stderr = std::str::from_utf8(&push.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).unwrap_or_else(|err| panic!("stderr JSON: {err}: {stderr}"));
    assert_eq!(envelope["kind"], "land_push_remote_missing");
    assert_json_recovery_advice_fields(&envelope, &envelope.to_string());
    assert!(
        envelope["preserved"]
            .as_str()
            .is_some_and(|preserved| preserved.contains("repository state"))
            && envelope["primary_command"]
                == "heddle land --thread feature/no-remote-push --no-push",
        "refusal should explain preservation and local land recovery: {envelope}"
    );
    assert_eq!(
        json(temp.path(), &["--output", "json", "status"])["current_state"],
        before_state
    );
    assert_eq!(git_stdout(temp.path(), &["rev-parse", "HEAD"]), before_git);
    assert_eq!(git_ref_snapshot(temp.path()), before_refs);
}

#[test]
fn git_overlay_matrix_ship_remote_without_push_refuses_before_mutation() {
    let temp = TempDir::new().unwrap();
    let origin = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(&["init", "--bare", "--initial-branch=main"], origin.path());
    std::fs::write(temp.path().join("README.md"), "base\n").unwrap();
    git_commit_all(temp.path(), "base");
    let origin_arg = origin.path().to_str().expect("origin path should be utf8");
    git(&["remote", "add", "origin", origin_arg], temp.path());
    git(&["push", "-u", "origin", "main"], temp.path());
    heddle_adopt(temp.path());

    let feature_path = temp.path().with_extension("feature-remote-without-push");
    json(
        temp.path(),
        &[
            "--output",
            "json",
            "start",
            "feature/remote-without-push",
            "--path",
            feature_path.to_str().unwrap(),
        ],
    );
    std::fs::write(feature_path.join("README.md"), "base\nfeature\n").unwrap();
    let ready = json(
        &feature_path,
        &[
            "--output",
            "json",
            "ready",
            "-m",
            "feature ready for remote without push",
        ],
    );
    assert_eq!(ready["status"], "completed");

    let before_state = json(temp.path(), &["--output", "json", "status"])["current_state"].clone();
    let before_git = git_stdout(temp.path(), &["rev-parse", "HEAD"]);
    let before_refs = git_ref_snapshot(temp.path());
    let land = heddle_output(
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/remote-without-push",
            "--remote",
            "origin",
        ],
        Some(temp.path()),
    )
    .expect("invoke land --remote");
    assert!(
        !land.status.success(),
        "land --remote without --push should refuse"
    );
    assert!(
        land.stdout.is_empty(),
        "JSON refusal should not emit partial stdout: {}",
        String::from_utf8_lossy(&land.stdout)
    );
    let stderr = std::str::from_utf8(&land.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).unwrap_or_else(|err| panic!("stderr JSON: {err}: {stderr}"));
    assert_eq!(envelope["kind"], "land_remote_requires_push");
    assert_eq!(
        envelope["primary_command"],
        "heddle land --thread feature/remote-without-push --push --remote origin"
    );
    assert_json_recovery_advice_fields(&envelope, &envelope.to_string());
    assert_eq!(
        json(temp.path(), &["--output", "json", "status"])["current_state"],
        before_state
    );
    assert_eq!(git_stdout(temp.path(), &["rev-parse", "HEAD"]), before_git);
    assert_eq!(git_ref_snapshot(temp.path()), before_refs);
}

#[test]
fn git_overlay_matrix_ship_push_failure_reports_partial_local_ship() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("README.md"), "base\n").unwrap();
    git_commit_all(temp.path(), "base");
    let missing_remote = temp.path().with_extension("missing-remote");
    git(
        &["remote", "add", "backup", missing_remote.to_str().unwrap()],
        temp.path(),
    );
    heddle_adopt(temp.path());

    let feature_path = temp.path().with_extension("feature-partial-push");
    json(
        temp.path(),
        &[
            "--output",
            "json",
            "start",
            "feature/partial-push",
            "--path",
            feature_path.to_str().unwrap(),
        ],
    );
    std::fs::write(feature_path.join("README.md"), "base\nfeature\n").unwrap();
    let ready = json(
        &feature_path,
        &["--output", "json", "ready", "-m", "feature partial push"],
    );
    assert_eq!(ready["status"], "completed");

    let preview = json(
        temp.path(),
        &[
            "--output",
            "json",
            "merge",
            "feature/partial-push",
            "--preview",
        ],
    );
    assert_eq!(
        preview["recommended_action"],
        "heddle land --thread feature/partial-push --no-push"
    );
    assert_eq!(
        preview["recommended_action_template"]["argv_template"],
        heddle_argv_json(["land", "--thread", "feature/partial-push", "--no-push"])
    );
    assert_eq!(
        preview["verification"]["recommended_action"],
        "heddle land --thread feature/partial-push --no-push"
    );

    let text_preview = heddle(
        &[
            "--output",
            "text",
            "merge",
            "feature/partial-push",
            "--preview",
            "--with-diff",
        ],
        Some(temp.path()),
    )
    .expect("text merge preview with diff should succeed");
    assert!(
        text_preview.contains("Next: heddle land --thread feature/partial-push --no-push"),
        "text merge preview should recommend local land first: {text_preview}"
    );
    assert!(
        !text_preview.contains("--push"),
        "text merge preview should not recommend pushing immediately: {text_preview}"
    );

    let before_state = json(temp.path(), &["--output", "json", "status"])["current_state"].clone();
    let before_git = git_stdout(temp.path(), &["rev-parse", "HEAD"]);
    let push = heddle_output(
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/partial-push",
            "--push",
            "--remote",
            "backup",
        ],
        Some(temp.path()),
    )
    .expect("invoke land --push");
    assert!(!push.status.success(), "push to missing remote should fail");
    assert!(
        push.stdout.is_empty(),
        "JSON partial failure should be a stderr envelope only: {}",
        String::from_utf8_lossy(&push.stdout)
    );
    let stderr = std::str::from_utf8(&push.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).unwrap_or_else(|err| panic!("stderr JSON: {err}: {stderr}"));
    assert_eq!(envelope["kind"], "land_push_partial_failure");
    assert_json_recovery_advice_fields(&envelope, &envelope.to_string());
    assert!(
        envelope["preserved"]
            .as_str()
            .is_some_and(|preserved| preserved.contains("completed steps: merge, checkpoint")),
        "partial failure should name completed work and recovery: {envelope}"
    );
    assert_eq!(envelope["primary_command"], "heddle undo");
    assert_eq!(
        envelope["recovery_commands"],
        serde_json::json!(["heddle undo", "heddle push backup"])
    );
    assert_ne!(
        json(temp.path(), &["--output", "json", "status"])["current_state"],
        before_state,
        "partial push failure should honestly report that local Heddle state moved"
    );
    assert_ne!(
        git_stdout(temp.path(), &["rev-parse", "HEAD"]),
        before_git,
        "partial push failure should honestly report that Git checkpoint moved"
    );

    let undo = json(temp.path(), &["--output", "json", "undo"]);
    assert_eq!(undo["action"], "undo");
    assert_eq!(
        json(temp.path(), &["--output", "json", "status"])["current_state"],
        before_state
    );
    assert_eq!(git_stdout(temp.path(), &["rev-parse", "HEAD"]), before_git);
}

#[test]
fn git_overlay_matrix_ship_no_push_refuses_known_upstream_drift_before_mutation() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let local = temp.path().join("local");
    let peer = temp.path().join("peer");
    std::fs::create_dir_all(&local).unwrap();
    let status = Command::new("git")
        .args([
            "init",
            "--bare",
            "--initial-branch=main",
            origin.to_str().unwrap(),
        ])
        .status()
        .expect("git init --bare should run");
    assert!(status.success());
    init_git_repo_with_branch(&local, "main");
    std::fs::write(local.join("README.md"), "base\n").unwrap();
    git_commit_all(&local, "base");
    git(
        &["remote", "add", "origin", origin.to_str().unwrap()],
        &local,
    );
    git(&["push", "-u", "origin", "main"], &local);
    heddle(&["adopt", "--ref", "main"], Some(&local)).expect("adopt local");

    git(
        &["clone", origin.to_str().unwrap(), peer.to_str().unwrap()],
        temp.path(),
    );
    git(&["config", "user.name", "Peer"], &peer);
    git(&["config", "user.email", "peer@example.com"], &peer);
    std::fs::write(peer.join("README.md"), "base\npeer\n").unwrap();
    git_commit_all(&peer, "peer");
    git(&["push", "origin", "main"], &peer);
    heddle(&["fetch", "origin"], Some(&local)).expect("fetch upstream drift");

    let feature_path = temp.path().join("isolated");
    json(
        &local,
        &[
            "--output",
            "json",
            "start",
            "isolated",
            "--path",
            feature_path.to_str().unwrap(),
        ],
    );
    std::fs::write(feature_path.join("LOCAL.md"), "local thread\n").unwrap();
    let ready = json(
        &feature_path,
        &["--output", "json", "ready", "-m", "isolated ready"],
    );
    assert_eq!(ready["status"], "completed");
    let before_status = json(&local, &["--output", "json", "status"]);
    assert_eq!(
        before_status["verification"]["remote_drift"],
        "remote_behind"
    );
    let before_state = before_status["current_state"].clone();
    let before_git = git_stdout(&local, &["rev-parse", "HEAD"]);
    let before_refs = git_ref_snapshot(&local);

    let land = heddle_output(
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "isolated",
            "--no-push",
        ],
        Some(&local),
    )
    .expect("invoke land --no-push with known upstream drift");
    assert!(
        !land.status.success(),
        "land should fail before landing when checkpoint cannot move Git safely"
    );
    assert!(
        land.stdout.is_empty(),
        "JSON refusal should not emit partial stdout: {}",
        String::from_utf8_lossy(&land.stdout)
    );
    let stderr = std::str::from_utf8(&land.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).unwrap_or_else(|err| panic!("stderr JSON: {err}: {stderr}"));
    assert_eq!(envelope["kind"], "land_requires_current_upstream");
    assert_eq!(envelope["primary_command"], "heddle pull");
    assert!(
        envelope["preserved"]
            .as_str()
            .is_some_and(|preserved| preserved.contains("left unchanged")),
        "refusal should clearly promise no local land occurred: {envelope}"
    );

    let after_status = json(&local, &["--output", "json", "status"]);
    assert_eq!(
        after_status["current_state"], before_state,
        "failed land must not fast-forward the parent Heddle thread"
    );
    assert_eq!(git_stdout(&local, &["rev-parse", "HEAD"]), before_git);
    assert_eq!(
        git_ref_snapshot(&local),
        before_refs,
        "failed land must not move visible Git refs"
    );
    let undo_list = json(&local, &["--output", "json", "undo", "--list"]);
    assert!(
        !undo_list
            .to_string()
            .contains("fast-forward isolated into main"),
        "failed land must not leave an undo batch for a merge that should not have happened: {undo_list}"
    );
}

#[test]
fn git_overlay_matrix_ship_refuses_index_lock_before_mutation() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("README.md"), "base\n").unwrap();
    git_commit_all(temp.path(), "base");
    heddle_adopt(temp.path());

    let feature_path = temp.path().with_extension("feature-index-lock");
    json(
        temp.path(),
        &[
            "--output",
            "json",
            "start",
            "feature/index-lock",
            "--path",
            feature_path.to_str().unwrap(),
        ],
    );
    std::fs::write(feature_path.join("README.md"), "base\nfeature\n").unwrap();
    let ready = json(
        &feature_path,
        &["--output", "json", "ready", "-m", "feature index lock"],
    );
    assert_eq!(ready["status"], "completed");
    let before_state = json(temp.path(), &["--output", "json", "status"])["current_state"].clone();
    let before_git = git_stdout(temp.path(), &["rev-parse", "HEAD"]);
    let before_refs = git_ref_snapshot(temp.path());
    std::fs::write(temp.path().join(".git/index.lock"), "stale lock").unwrap();

    let land = heddle_output(
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/index-lock",
            "--no-push",
        ],
        Some(temp.path()),
    )
    .expect("invoke land with index lock");
    assert!(
        !land.status.success(),
        "land should fail before landing when checkpoint preflight sees index lock"
    );
    assert!(land.stdout.is_empty());
    let stderr = std::str::from_utf8(&land.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).unwrap_or_else(|err| panic!("stderr JSON: {err}: {stderr}"));
    assert_eq!(envelope["kind"], "land_checkpoint_preflight_blocked");
    assert_eq!(envelope["primary_command"], "heddle status");
    assert_eq!(
        json(temp.path(), &["--output", "json", "status"])["current_state"],
        before_state,
        "failed land must not fast-forward Heddle state"
    );
    assert_eq!(git_stdout(temp.path(), &["rev-parse", "HEAD"]), before_git);
    assert_eq!(git_ref_snapshot(temp.path()), before_refs);
}

#[test]
fn git_overlay_matrix_manual_git_merge_commit_after_bootstrap_commands() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("shared.txt"), "base\n").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(
        &["bridge", "git", "import", "--ref", "feature/drop-in"],
        Some(temp.path()),
    )
    .unwrap();

    git(&["checkout", "-b", "support/merge"], temp.path());
    std::fs::write(temp.path().join("side.txt"), "side branch\n").unwrap();
    git_commit_all(temp.path(), "side branch work");

    git(&["checkout", "feature/drop-in"], temp.path());
    std::fs::write(temp.path().join("main.txt"), "main branch\n").unwrap();
    git_commit_all(temp.path(), "main branch work");

    git(
        &[
            "merge",
            "--no-ff",
            "support/merge",
            "-m",
            "merge support branch",
        ],
        temp.path(),
    );

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["thread"], "feature/drop-in");
    assert_eq!(
        status["recommended_action"],
        "heddle adopt --ref feature/drop-in"
    );
    assert!(
        status["changes"]["added"].as_array().unwrap().is_empty()
            && status["changes"]["modified"].as_array().unwrap().is_empty(),
        "manual Git merge commits leave Git clean and should be shown as import drift, not unsaved work: {status}"
    );
    assert_eq!(status["changed_path_count"], 0);

    let log = json(temp.path(), &["log", "--output", "json"]);
    assert!(
        !log["states"].as_array().unwrap().is_empty(),
        "log should stay coherent after a manual Git merge commit: {log}"
    );

    let same_state_diff = json(temp.path(), &["diff", "HEAD", "HEAD"]);
    assert_eq!(same_state_diff["stats"]["files_changed"], 0);

    let ready = json(temp.path(), &["--output", "json", "ready"]);
    assert!(
        ready["captured"].is_boolean(),
        "ready should remain well-formed after a manual Git merge commit: {ready}"
    );
}

#[test]
fn git_overlay_matrix_side_only_import_is_available_not_next_action() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    git_commit_all(temp.path(), "seed");

    git(&["checkout", "-b", "side"], temp.path());
    std::fs::write(temp.path().join("side.txt"), "side\n").unwrap();
    git_commit_all(temp.path(), "side work");

    git(&["checkout", "main"], temp.path());
    std::fs::write(temp.path().join("main.txt"), "main\n").unwrap();
    git_commit_all(temp.path(), "main work");
    git(
        &["merge", "--no-ff", "side", "-m", "merge side"],
        temp.path(),
    );

    heddle(&["adopt", "--ref", "main"], Some(temp.path())).unwrap();

    let verify = json(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(verify["verified"], true);
    assert_eq!(verify["status"], "clean");
    assert_eq!(
        verify["recommended_action"],
        Value::Null,
        "side-only import availability should not hijack verified mainline flow: {verify}"
    );
    let mapping = verify["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "Mapping")
        .unwrap_or_else(|| panic!("verify should include Mapping check: {verify}"));
    assert_eq!(mapping["clean"], true);
    assert_eq!(mapping["recommended_action"], Value::Null);

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["verification"]["verified"], true);
    assert_eq!(status["verification"]["status"], "clean");
    assert_eq!(status["output_kind"], "status");
    assert_eq!(status["recommended_action"], Value::Null);

    let thread_list = json(temp.path(), &["thread", "list", "--output", "json"]);
    assert_eq!(thread_list["output_kind"], "thread_list");
    assert_eq!(thread_list["recommended_action"], Value::Null);
    assert!(
        thread_list["threads"]
            .as_array()
            .unwrap()
            .iter()
            .all(|thread| thread["name"] != "side"),
        "side-only refs should not be modeled as active threads: {thread_list}"
    );
    assert_eq!(thread_list["available_git_refs"][0]["name"], "side");
    assert_eq!(
        thread_list["available_git_refs"][0]["recommended_action"],
        "heddle adopt --ref side"
    );
    assert_eq!(
        thread_list["available_git_refs"][0]["recommended_action_template"]["argv_template"],
        heddle_argv_json(["adopt", "--ref", "side"])
    );

    let workspace = json(temp.path(), &["workspace", "show", "--output", "json"]);
    assert_eq!(workspace["output_kind"], "workspace_summary");
    assert_eq!(workspace["recommended_action"], Value::Null);
    assert!(
        workspace["groups"]
            .as_array()
            .unwrap()
            .iter()
            .all(|group| group["id"] != "available_git_refs"),
        "available Git refs should be a typed top-level JSON field, not thread-shaped groups: {workspace}"
    );
    assert_eq!(workspace["available_git_refs"][0]["name"], "side");
    assert_eq!(
        workspace["available_git_refs"][0]["recommended_action_template"]["argv_template"],
        heddle_argv_json(["adopt", "--ref", "side"])
    );

    let bridge = json(
        temp.path(),
        &["bridge", "git", "status", "--output", "json"],
    );
    assert_eq!(bridge["verification"]["verified"], true);
    assert_eq!(bridge["verification"]["status"], "clean");
    assert_eq!(bridge["output_kind"], "bridge_git_status");
    assert_eq!(bridge["recommended_action"], Value::Null);
    assert_eq!(
        bridge["git_overlay_import_hint"],
        Value::Null,
        "a side branch whose tip was already imported through main should not make the bridge report missing import work: {bridge}"
    );

    for (label, args) in [
        ("thread list", &["thread", "list", "--output", "text"][..]),
        ("workspace", &["workspace", "show", "--output", "text"][..]),
    ] {
        let text = heddle(args, Some(temp.path())).unwrap();
        assert!(
            text.contains("Optional Git-only branch available: side")
                || text.contains("Optional Git-only branches"),
            "{label} should use optional Git-only branch language: {text}"
        );
        assert!(
            text.contains("adopt when you want to work on this branch in Heddle"),
            "{label} should explain optional Git-only branch adoption without sounding blocked: {text}"
        );
        assert!(
            !text.contains("Available Git refs") && !text.contains("optional import:"),
            "{label} should avoid implementation-shaped Git ref copy: {text}"
        );
    }

    let status_text = heddle(&["status", "--output", "text", "-v"], Some(temp.path())).unwrap();
    assert!(
        status_text.contains("Health: clean")
            && !status_text.contains("Setup needed")
            && !status_text.contains("Next step: heddle adopt --ref side"),
        "current-branch status should stay focused on the verified checkout: {status_text}"
    );
    let bridge_text = heddle(
        &["bridge", "git", "status", "--output", "text"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        bridge_text.contains("Git import: in sync")
            && !bridge_text.contains("heddle adopt --ref side")
            && !bridge_text.contains("Next step: heddle adopt --ref side"),
        "bridge status should report imported history as in sync and leave optional branch adoption to thread/workspace views: {bridge_text}"
    );

    std::fs::write(temp.path().join("scratch.txt"), "dirty\n").unwrap();
    let dirty_thread_list = json(temp.path(), &["thread", "list", "--output", "json"]);
    assert_eq!(
        dirty_thread_list["recommended_action"], "heddle commit -m \"...\"",
        "dirty checkout should keep the primary action focused on saved work: {dirty_thread_list}"
    );
    assert_eq!(dirty_thread_list["available_git_refs"][0]["name"], "side");
    assert_eq!(
        dirty_thread_list["available_git_refs"][0]["recommended_action"], "heddle adopt --ref side",
        "available Git refs must keep executable adopt actions even when verification is blocked: {dirty_thread_list}"
    );
    assert_eq!(
        dirty_thread_list["available_git_refs"][0]["recommended_action_template"]["argv_template"],
        heddle_argv_json(["adopt", "--ref", "side"])
    );

    let dirty_workspace = json(temp.path(), &["workspace", "show", "--output", "json"]);
    assert_eq!(
        dirty_workspace["available_git_refs"][0]["recommended_action"],
        dirty_thread_list["available_git_refs"][0]["recommended_action"],
        "thread list and workspace show should agree on optional Git ref adoption while blocked: thread={dirty_thread_list} workspace={dirty_workspace}"
    );
    assert_eq!(
        dirty_workspace["available_git_refs"][0]["recommended_action_template"]["argv_template"],
        dirty_thread_list["available_git_refs"][0]["recommended_action_template"]["argv_template"],
        "thread list and workspace show argv should match for optional Git ref adoption while blocked: thread={dirty_thread_list} workspace={dirty_workspace}"
    );
}

#[test]
fn git_overlay_matrix_imported_branch_git_only_advance_reappears_in_import_hint() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed branch");

    git(&["checkout", "-b", "support/alpha"], temp.path());
    std::fs::write(temp.path().join("alpha.txt"), "alpha one\n").unwrap();
    git_commit_all(temp.path(), "alpha one");
    git(&["checkout", "feature/drop-in"], temp.path());

    let import_output = heddle(&["bridge", "import", "--path", "."], Some(temp.path())).unwrap();
    assert!(
        import_output.contains("branches") || import_output.contains("\"branches_synced\""),
        "bridge import should report branch sync activity: {import_output}"
    );

    let threads_after_import = json(temp.path(), &["thread", "list", "--output", "json"]);
    assert!(
        threads_after_import["threads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|thread| thread["name"] == "support/alpha"),
        "thread list should include imported branch after bridge import: {threads_after_import}"
    );

    git(&["checkout", "support/alpha"], temp.path());
    std::fs::write(temp.path().join("alpha.txt"), "alpha two\n").unwrap();
    git_commit_all(temp.path(), "alpha two");
    git(&["checkout", "feature/drop-in"], temp.path());

    let status = json(
        temp.path(),
        &["bridge", "git", "status", "--output", "json"],
    );
    let missing = status["git_overlay_import_hint"]["missing_branches"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert!(
        missing.contains(&"support/alpha"),
        "Git-only branch advancement after import should reappear in the import hint: {status}"
    );

    let bridge = json(
        temp.path(),
        &["bridge", "git", "status", "--output", "json"],
    );
    assert_eq!(bridge["git_overlay_import_hint"]["missing_branch_count"], 1);
}

#[test]
fn git_overlay_matrix_imported_branch_delete_and_recreate_same_name_reappears_in_hint() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed branch");

    git(&["checkout", "-b", "support/reborn"], temp.path());
    std::fs::write(temp.path().join("reborn.txt"), "first life\n").unwrap();
    git_commit_all(temp.path(), "first reborn");
    git(&["checkout", "feature/drop-in"], temp.path());

    let _ = heddle(&["bridge", "import", "--path", "."], Some(temp.path())).unwrap();

    git(&["branch", "-D", "support/reborn"], temp.path());
    git(&["checkout", "-b", "support/reborn"], temp.path());
    std::fs::write(temp.path().join("reborn.txt"), "second life\n").unwrap();
    git_commit_all(temp.path(), "second reborn");
    git(&["checkout", "feature/drop-in"], temp.path());

    let status = json(
        temp.path(),
        &["bridge", "git", "status", "--output", "json"],
    );
    let missing = status["git_overlay_import_hint"]["missing_branches"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert!(
        missing.contains(&"support/reborn"),
        "recreating an imported branch with the same name should reappear as a Git-only evolution: {status}"
    );

    let bridge_again = json(
        temp.path(),
        &["bridge", "git", "status", "--output", "json"],
    );
    assert_eq!(
        bridge_again["git_overlay_import_hint"]["missing_branch_count"],
        1
    );
}

#[test]
fn git_overlay_matrix_git_add_dot_does_not_stage_heddle_sidecar() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle(&["init"], Some(temp.path())).unwrap();

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["repository_capability"], "git-overlay");

    std::fs::write(temp.path().join("tracked.txt"), "tracked updated\n").unwrap();
    git(&["add", "."], temp.path());

    let staged = Command::new("git")
        .args(["diff", "--cached", "--name-only"])
        .current_dir(temp.path())
        .output()
        .expect("git diff --cached should run");
    assert!(staged.status.success(), "git diff --cached should succeed");
    let staged_stdout = String::from_utf8_lossy(&staged.stdout).to_string();
    let staged_paths = staged_stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    assert!(
        staged_paths.contains(&"tracked.txt"),
        "expected tracked work to stage normally: {:?}",
        staged_paths
    );
    assert!(
        staged_paths.iter().all(|path| !path.starts_with(".heddle")),
        "git add . should not stage the Heddle sidecar in a Git-overlay repo: {:?}",
        staged_paths
    );
}

#[test]
fn git_overlay_matrix_rebase_and_cherry_pick_sequences_remain_coherent() {
    let rebase_repo = TempDir::new().unwrap();
    init_git_repo_with_branch(rebase_repo.path(), "feature/drop-in");
    std::fs::write(rebase_repo.path().join("base.txt"), "base\n").unwrap();
    git_commit_all(rebase_repo.path(), "seed branch");
    heddle_adopt(rebase_repo.path());

    git(&["checkout", "-b", "support/rebase"], rebase_repo.path());
    std::fs::write(rebase_repo.path().join("clash.txt"), "support rebase\n").unwrap();
    git_commit_all(rebase_repo.path(), "support rebase");

    git(&["checkout", "feature/drop-in"], rebase_repo.path());
    std::fs::write(rebase_repo.path().join("clash.txt"), "main rebase\n").unwrap();
    git_commit_all(rebase_repo.path(), "main rebase");
    git(&["checkout", "support/rebase"], rebase_repo.path());

    let rebase = Command::new("git")
        .args(["rebase", "feature/drop-in"])
        .current_dir(rebase_repo.path())
        .output()
        .expect("git rebase should run");
    assert!(
        !rebase.status.success(),
        "expected conflicting rebase to stop for manual resolution: {}",
        String::from_utf8_lossy(&rebase.stderr)
    );

    let status = json(rebase_repo.path(), &["status", "--output", "json"]);
    assert_eq!(status["repository_capability"], "git-overlay");
    assert!(
        status["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "clash.txt")
            || status["changes"]["modified"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "clash.txt"),
        "status should stay coherent during rebase conflict: {status}"
    );

    let diagnose = json(rebase_repo.path(), &["doctor", "--output", "json"]);
    assert_eq!(diagnose["repository_capability"], "git-overlay");

    let worktree = json(
        rebase_repo.path(),
        &["workspace", "show", "--output", "json"],
    );
    assert_eq!(worktree["repository_capability"], "git-overlay");

    git(&["rebase", "--abort"], rebase_repo.path());

    let cherry_repo = TempDir::new().unwrap();
    init_git_repo_with_branch(cherry_repo.path(), "feature/drop-in");
    std::fs::write(cherry_repo.path().join("conflict.txt"), "base\n").unwrap();
    git_commit_all(cherry_repo.path(), "seed branch");
    heddle_adopt(cherry_repo.path());

    git(&["checkout", "-b", "support/cherry"], cherry_repo.path());
    std::fs::write(cherry_repo.path().join("extra.txt"), "support extra\n").unwrap();
    git_commit_all(cherry_repo.path(), "support extra");
    std::fs::write(cherry_repo.path().join("conflict.txt"), "support cherry\n").unwrap();
    git_commit_all(cherry_repo.path(), "support cherry");

    let cherry_commit = {
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(cherry_repo.path())
            .output()
            .expect("git rev-parse should run");
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };

    git(&["checkout", "feature/drop-in"], cherry_repo.path());
    std::fs::write(cherry_repo.path().join("conflict.txt"), "main cherry\n").unwrap();
    git_commit_all(cherry_repo.path(), "main cherry");
    heddle(
        &["bridge", "git", "import", "--ref", "feature/drop-in"],
        Some(cherry_repo.path()),
    )
    .unwrap();

    let cherry_pick = Command::new("git")
        .args(["cherry-pick", &cherry_commit])
        .current_dir(cherry_repo.path())
        .output()
        .expect("git cherry-pick should run");
    assert!(
        !cherry_pick.status.success(),
        "expected conflicting cherry-pick to stop for manual resolution"
    );

    let cherry_status = json(cherry_repo.path(), &["status", "--output", "json"]);
    assert_eq!(cherry_status["thread"], "feature/drop-in");
    assert!(
        cherry_status["changes"]["modified"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "conflict.txt"),
        "status should stay coherent during cherry-pick conflict: {cherry_status}"
    );

    let cherry_show = json(cherry_repo.path(), &["show", "HEAD", "--output", "json"]);
    assert!(cherry_show["change_id"].as_str().is_some());

    let before_capture_head = git_stdout(cherry_repo.path(), &["rev-parse", "HEAD"]);
    let before_capture_state = cherry_status["current_state"]
        .as_str()
        .expect("status should report current Heddle state")
        .to_string();
    let capture = heddle_output(
        &[
            "--output",
            "json",
            "capture",
            "-m",
            "should not preserve sequencer",
        ],
        Some(cherry_repo.path()),
    )
    .expect("capture should run");
    assert!(
        !capture.status.success(),
        "capture should refuse raw Git sequencer state"
    );
    assert!(capture.stdout.is_empty());
    let stderr = std::str::from_utf8(&capture.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("raw Git capture refusal should be JSON");
    assert_eq!(envelope["kind"], "raw_git_operation_in_progress");
    assert_eq!(envelope["primary_command"], "heddle bridge git status");
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("Git-compatible tool that started it")
                && hint.contains("heddle verify")
                && !hint.contains("heddle adopt --ref <branch>")),
        "raw Git capture refusal should explain the external sequencer recovery: {stderr}"
    );
    assert!(cherry_repo.path().join(".git/CHERRY_PICK_HEAD").exists());
    assert_eq!(
        git_stdout(cherry_repo.path(), &["rev-parse", "HEAD"]),
        before_capture_head
    );
    assert_eq!(
        json(cherry_repo.path(), &["status", "--output", "json"])["current_state"],
        before_capture_state,
        "raw Git capture refusal must leave Heddle HEAD unchanged"
    );

    git(&["cherry-pick", "--abort"], cherry_repo.path());
}

#[test]
fn git_overlay_matrix_stale_ship_manual_resolution_then_retry_ships() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("conflict.txt"), "base\n").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle_adopt(temp.path());

    let started = json(
        temp.path(),
        &[
            "--output",
            "json",
            "start",
            "feature/manual-recover",
            "--workspace",
            "materialized",
        ],
    );
    let thread_path = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());

    std::fs::write(thread_path.join("conflict.txt"), "thread change\n").unwrap();
    heddle(&["capture", "-m", "thread change"], Some(&thread_path)).unwrap();

    std::fs::write(temp.path().join("conflict.txt"), "main change\n").unwrap();
    heddle(&["capture", "-m", "main change"], Some(temp.path())).unwrap();

    let blocked = json(
        temp.path(),
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/manual-recover",
        ],
    );
    assert_eq!(blocked["status"], "blocked");
    assert_operator_json_contract(&blocked, "land");

    let refresh_error = heddle(
        &[
            "--output",
            "json",
            "thread",
            "refresh",
            "feature/manual-recover",
        ],
        Some(temp.path()),
    )
    .expect_err("refresh should materialize durable conflict state before manual resolution");
    assert!(
        refresh_error.contains("thread_refresh_conflicted")
            && refresh_error.contains(thread_path.to_str().unwrap()),
        "refresh conflict should point at the thread checkout for resolution: {refresh_error}"
    );
    std::fs::write(
        thread_path.join("conflict.txt"),
        "main change\nthread change\n",
    )
    .unwrap();
    heddle(&["resolve", "conflict.txt"], Some(&thread_path)).unwrap();
    let continued = json(&thread_path, &["--output", "json", "continue"]);
    assert_eq!(continued["status"], "continued");
    assert_operator_json_contract(&continued, "merge");
    assert!(
        continued["recommended_action"]
            .as_str()
            .is_some_and(|action| action.contains("land")),
        "continue should hand the operator back to the parent land flow: {continued}"
    );

    let after_continue = json(
        temp.path(),
        &[
            "thread",
            "show",
            "feature/manual-recover",
            "--output",
            "json",
        ],
    );
    assert_eq!(after_continue["freshness"], "current", "{after_continue}");
    assert_eq!(after_continue["thread_state"], "ready", "{after_continue}");
    assert_eq!(
        after_continue["integration_policy_result"]["status"], "manual_resolved",
        "{after_continue}"
    );
    assert!(
        !after_continue["recommended_action"]
            .as_str()
            .unwrap_or("")
            .contains("thread refresh"),
        "manual conflict resolution should not leave parent thread advice stuck on refresh: {after_continue}"
    );
    let resolved_again = json(
        temp.path(),
        &[
            "--output",
            "json",
            "thread",
            "resolve",
            "feature/manual-recover",
        ],
    );
    assert_eq!(resolved_again["status"], "completed", "{resolved_again}");
    assert!(
        resolved_again["message"]
            .as_str()
            .is_some_and(|message| message.contains("manual resolution recorded")
                && !message.contains("requires a manual follow-up")),
        "completed thread resolve should not read like it is still blocked: {resolved_again}"
    );

    let retry_ship = json(
        temp.path(),
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/manual-recover",
        ],
    );
    assert_eq!(retry_ship["status"], "landed");
    assert_operator_json_contract(&retry_ship, "land");
    assert_eq!(retry_ship["checkpointed"], true);
    assert!(retry_ship["git_commit"].as_str().is_some());
    let expected_next_action = if retry_ship["verification"]["recommended_action"] == "heddle push"
    {
        "heddle push"
    } else {
        "heddle thread cleanup --merged --dry-run"
    };
    assert_eq!(retry_ship["next_action"], expected_next_action);
    assert_eq!(retry_ship["recommended_action"], expected_next_action);
}

#[test]
fn git_overlay_matrix_stale_ship_manual_resolution_pushes_when_requested() {
    let temp = TempDir::new().unwrap();
    let origin = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    git(
        &["init", "--bare", "--initial-branch=feature/drop-in"],
        origin.path(),
    );
    std::fs::write(temp.path().join("conflict.txt"), "base\n").unwrap();
    git_commit_all(temp.path(), "seed branch");
    let origin_arg = origin.path().to_str().expect("origin path should be utf8");
    git(&["remote", "add", "origin", origin_arg], temp.path());
    git(&["push", "-u", "origin", "feature/drop-in"], temp.path());
    heddle_adopt(temp.path());

    let started = json(
        temp.path(),
        &[
            "--output",
            "json",
            "start",
            "feature/manual-push",
            "--workspace",
            "materialized",
        ],
    );
    let thread_path = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());

    std::fs::write(thread_path.join("conflict.txt"), "thread change\n").unwrap();
    heddle(&["capture", "-m", "thread change"], Some(&thread_path)).unwrap();

    std::fs::write(temp.path().join("conflict.txt"), "main change\n").unwrap();
    heddle(&["capture", "-m", "main change"], Some(temp.path())).unwrap();

    let blocked = json(
        temp.path(),
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/manual-push",
        ],
    );
    assert_eq!(blocked["status"], "blocked");
    assert_operator_json_contract(&blocked, "land");

    let refresh_error = heddle(
        &[
            "--output",
            "json",
            "thread",
            "refresh",
            "feature/manual-push",
        ],
        Some(temp.path()),
    )
    .expect_err("refresh should materialize durable conflict state before manual resolution");
    assert!(
        refresh_error.contains("thread_refresh_conflicted")
            && refresh_error.contains(thread_path.to_str().unwrap()),
        "refresh conflict should point at the thread checkout for resolution: {refresh_error}"
    );
    std::fs::write(
        thread_path.join("conflict.txt"),
        "main change\nthread change\n",
    )
    .unwrap();
    heddle(&["resolve", "conflict.txt"], Some(&thread_path)).unwrap();
    let continued = json(&thread_path, &["--output", "json", "continue"]);
    assert_eq!(continued["status"], "continued");
    assert_operator_json_contract(&continued, "merge");
    heddle(
        &["thread", "resolve", "feature/manual-push"],
        Some(temp.path()),
    )
    .unwrap();

    let retry_ship = json(
        temp.path(),
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/manual-push",
            "--push",
            "--remote",
            "origin",
        ],
    );
    assert_eq!(retry_ship["status"], "landed");
    assert_operator_json_contract(&retry_ship, "land");
    assert_eq!(retry_ship["checkpointed"], true);
    assert_eq!(retry_ship["pushed"], true);
    assert_eq!(retry_ship["pushed_remote"], "origin");
    assert_eq!(
        retry_ship["chosen_path"],
        "capture_sync_manual_resolution_checkpoint_push"
    );
    assert!(
        retry_ship["performed_steps"]
            .as_array()
            .expect("performed_steps array")
            .iter()
            .any(|step| step == "push"),
        "manual-resolution land --push should record the push step: {retry_ship}"
    );
    assert!(
        !retry_ship["skipped_steps"]
            .as_array()
            .expect("skipped_steps array")
            .iter()
            .any(|step| step == "push(not requested)"),
        "manual-resolution land --push must not claim the push was not requested: {retry_ship}"
    );
    assert_eq!(
        retry_ship["recommended_action"],
        "heddle thread cleanup --merged --dry-run"
    );
    assert_eq!(
        git_stdout(origin.path(), &["rev-parse", "refs/heads/feature/drop-in"]),
        git_stdout(temp.path(), &["rev-parse", "HEAD"]),
        "explicit land --push --remote origin should update the remote branch"
    );
    let verify = json(temp.path(), &["--output", "json", "verify"]);
    assert_eq!(verify["verified"], true);
    assert_eq!(verify["status"], "clean");
    assert_eq!(verify["recommended_action"], Value::Null);
}

#[test]
fn git_overlay_matrix_native_git_worktree_bootstraps_cleanly() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed branch");

    let worktree_path = temp.path().join("git-worktrees/support");
    std::fs::create_dir_all(worktree_path.parent().unwrap()).unwrap();
    git(
        &[
            "worktree",
            "add",
            "-b",
            "support/native-worktree",
            worktree_path.to_str().unwrap(),
        ],
        temp.path(),
    );

    heddle_adopt(&worktree_path);
    std::fs::write(worktree_path.join("native.txt"), "native worktree\n").unwrap();

    let status = json(&worktree_path, &["status", "--output", "json"]);
    assert_eq!(status["thread"], "support/native-worktree");
    assert_eq!(status["repository_capability"], "git-overlay");
    assert!(
        status["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "native.txt")
    );

    let workspace = json(&worktree_path, &["workspace", "show", "--output", "json"]);
    assert_eq!(workspace["current_thread"], "support/native-worktree");

    let ready = json(
        &worktree_path,
        &["--output", "json", "ready", "-m", "native worktree ready"],
    );
    assert_operator_json_contract(&ready, "ready");
    assert_eq!(ready["captured"], true);
}

#[test]
fn git_overlay_matrix_current_branch_rename_updates_active_thread_views() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle(&["init"], Some(temp.path())).unwrap();
    let _ = json(temp.path(), &["status", "--output", "json"]);

    git(&["branch", "-m", "feature/renamed-current"], temp.path());

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["thread"], "feature/renamed-current");

    let thread_list = json(temp.path(), &["thread", "list", "--output", "json"]);
    assert_eq!(thread_list["current"], "feature/renamed-current");

    let workspace = json(temp.path(), &["workspace", "show", "--output", "json"]);
    assert_eq!(workspace["current_thread"], "feature/renamed-current");
}

#[test]
fn git_overlay_matrix_imported_branch_merge_commit_drift_reappears_in_hint() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed branch");

    git(&["checkout", "-b", "support/merge-drift"], temp.path());
    std::fs::write(temp.path().join("merge.txt"), "support base\n").unwrap();
    git_commit_all(temp.path(), "support base");
    git(&["checkout", "feature/drop-in"], temp.path());

    let _ = heddle(&["bridge", "import", "--path", "."], Some(temp.path())).unwrap();

    git(&["checkout", "support/merge-drift"], temp.path());
    git(&["checkout", "-b", "support/merge-drift-side"], temp.path());
    std::fs::write(temp.path().join("side.txt"), "side merge\n").unwrap();
    git_commit_all(temp.path(), "side merge");
    git(&["checkout", "support/merge-drift"], temp.path());
    std::fs::write(temp.path().join("merge.txt"), "support advanced\n").unwrap();
    git_commit_all(temp.path(), "support advanced");
    git(
        &[
            "merge",
            "--no-ff",
            "support/merge-drift-side",
            "-m",
            "merge side into imported branch",
        ],
        temp.path(),
    );
    git(&["checkout", "feature/drop-in"], temp.path());

    let status = json(
        temp.path(),
        &["bridge", "git", "status", "--output", "json"],
    );
    let missing = status["git_overlay_import_hint"]["missing_branches"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert!(
        missing.contains(&"support/merge-drift"),
        "imported branch whose Git tip became a merge commit should reappear in the drift hint: {status}"
    );
}

#[test]
fn git_overlay_matrix_in_progress_operations_surface_consistently() {
    let rebase_repo = TempDir::new().unwrap();
    init_git_repo_with_branch(rebase_repo.path(), "feature/drop-in");
    std::fs::write(rebase_repo.path().join("base.txt"), "base\n").unwrap();
    git_commit_all(rebase_repo.path(), "seed branch");
    heddle(&["init"], Some(rebase_repo.path())).unwrap();
    let _ = json(rebase_repo.path(), &["status", "--output", "json"]);

    git(&["checkout", "-b", "support/rebase"], rebase_repo.path());
    std::fs::write(rebase_repo.path().join("clash.txt"), "support rebase\n").unwrap();
    git_commit_all(rebase_repo.path(), "support rebase");
    git(&["checkout", "feature/drop-in"], rebase_repo.path());
    std::fs::write(rebase_repo.path().join("clash.txt"), "main rebase\n").unwrap();
    git_commit_all(rebase_repo.path(), "main rebase");
    git(&["checkout", "support/rebase"], rebase_repo.path());
    let rebase = Command::new("git")
        .args(["rebase", "feature/drop-in"])
        .current_dir(rebase_repo.path())
        .output()
        .expect("git rebase should run");
    assert!(!rebase.status.success());

    let status = json(rebase_repo.path(), &["status", "--output", "json"]);
    assert_eq!(status["operation"]["scope"], "git");
    assert_eq!(status["operation"]["kind"], "rebase");
    assert_eq!(
        status["operation"]["next_action"],
        "heddle bridge git status"
    );
    let diagnose = json(rebase_repo.path(), &["doctor", "--output", "json"]);
    assert_eq!(diagnose["operation"]["kind"], "rebase");
    let workspace = json(
        rebase_repo.path(),
        &["workspace", "show", "--output", "json"],
    );
    assert_eq!(workspace["operation"]["kind"], "rebase");
    let thread_list = json(rebase_repo.path(), &["thread", "list", "--output", "json"]);
    let current = thread_list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["is_current"] == true)
        .expect("current thread should be present");
    assert_eq!(current["operation"]["kind"], "rebase");
    git(&["rebase", "--abort"], rebase_repo.path());

    let revert_repo = TempDir::new().unwrap();
    init_git_repo_with_branch(revert_repo.path(), "feature/drop-in");
    std::fs::write(revert_repo.path().join("tracked.txt"), "base\n").unwrap();
    git_commit_all(revert_repo.path(), "seed branch");
    heddle(&["init"], Some(revert_repo.path())).unwrap();
    let _ = json(revert_repo.path(), &["status", "--output", "json"]);
    std::fs::write(revert_repo.path().join("tracked.txt"), "main change\n").unwrap();
    git_commit_all(revert_repo.path(), "main change");
    std::fs::write(revert_repo.path().join("tracked.txt"), "follow-up change\n").unwrap();
    git_commit_all(revert_repo.path(), "follow-up change");

    let revert = Command::new("git")
        .args(["revert", "--no-commit", "HEAD"])
        .current_dir(revert_repo.path())
        .output()
        .expect("git revert should run");
    assert!(
        revert.status.success(),
        "git revert --no-commit should succeed"
    );

    let revert_status = json(revert_repo.path(), &["status", "--output", "json"]);
    assert_eq!(revert_status["operation"]["kind"], "revert");
    assert_eq!(
        revert_status["operation"]["next_action"],
        "heddle bridge git status"
    );
    git(&["revert", "--abort"], revert_repo.path());

    let bisect_repo = TempDir::new().unwrap();
    init_git_repo_with_branch(bisect_repo.path(), "feature/drop-in");
    std::fs::write(bisect_repo.path().join("tracked.txt"), "base\n").unwrap();
    git_commit_all(bisect_repo.path(), "seed branch");
    let _ = json(bisect_repo.path(), &["status", "--output", "json"]);
    seed_heddle_bisect_state(bisect_repo.path());
    let bisect_status = json(bisect_repo.path(), &["status", "--output", "json"]);
    assert_eq!(bisect_status["operation"]["scope"], "heddle");
    assert_eq!(bisect_status["operation"]["kind"], "bisect");
    assert_eq!(
        bisect_status["operation"]["next_action"],
        "heddle abort"
    );
}

#[test]
fn git_overlay_matrix_native_worktree_branch_switch_and_remote_drift_surface_cleanly() {
    let remote = TempDir::new().unwrap();
    git(
        &["init", "--bare", remote.path().to_str().unwrap()],
        remote.path(),
    );

    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    git(
        &["remote", "add", "origin", remote.path().to_str().unwrap()],
        temp.path(),
    );
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["push", "-u", "origin", "feature/drop-in"], temp.path());
    heddle_adopt(temp.path());

    let worktree_path = temp.path().join("git-worktrees/support");
    std::fs::create_dir_all(worktree_path.parent().unwrap()).unwrap();
    git(
        &[
            "worktree",
            "add",
            "-b",
            "support/native-worktree",
            worktree_path.to_str().unwrap(),
        ],
        temp.path(),
    );
    std::fs::write(worktree_path.join("native.txt"), "native worktree\n").unwrap();
    heddle_adopt(&worktree_path);
    let worktree_status = json(&worktree_path, &["status", "--output", "json"]);
    assert_eq!(worktree_status["thread"], "support/native-worktree");
    assert_eq!(
        worktree_status["remote_tracking"]["upstream"], "",
        "new native Git worktree branch should surface as untracked, not disappear: {worktree_status}"
    );

    git(
        &["checkout", "-b", "support/renamed-switch"],
        &worktree_path,
    );
    std::fs::write(worktree_path.join("renamed.txt"), "renamed branch\n").unwrap();
    let switched = json(&worktree_path, &["workspace", "show", "--output", "json"]);
    assert_eq!(switched["current_thread"], "support/renamed-switch");

    let other = TempDir::new().unwrap();
    git(
        &[
            "clone",
            remote.path().to_str().unwrap(),
            other.path().to_str().unwrap(),
        ],
        temp.path(),
    );
    // Clone does not inherit user identity from the remote; configure it
    // explicitly so `git commit` succeeds on CI runners without a global
    // git config.
    git(&["config", "user.name", "Heddle Test"], other.path());
    git(
        &["config", "user.email", "heddle@example.com"],
        other.path(),
    );
    git(&["checkout", "feature/drop-in"], other.path());
    std::fs::write(other.path().join("tracked.txt"), "remote advanced\n").unwrap();
    git_commit_all(other.path(), "remote advance");
    git(&["push", "origin", "feature/drop-in"], other.path());
    git(&["fetch", "origin"], temp.path());

    let root_status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(root_status["thread"], "feature/drop-in");
    assert_eq!(root_status["remote_tracking"]["branch"], "feature/drop-in");
    assert_eq!(root_status["remote_tracking"]["behind"], 1);
    assert_eq!(root_status["remote_tracking"]["next_action"], "heddle pull");

    let thread_list = json(temp.path(), &["thread", "list", "--output", "json"]);
    let current = thread_list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["is_current"] == true)
        .expect("current thread should be present");
    assert_eq!(current["remote_tracking"]["behind"], 1);
}

#[test]
fn git_overlay_matrix_continue_and_abort_unify_operator_flow() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("conflict.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();
    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("conflict.txt"), "feature version\n").unwrap();
    heddle(&["capture", "-m", "Feature change"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("conflict.txt"), "main version\n").unwrap();
    heddle(&["capture", "-m", "Main change"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();

    let merge_output = start_conflicted_heddle_merge(temp.path());
    assert!(
        merge_output.contains("Conflict") || temp.path().join(".heddle/MERGE_STATE").exists(),
        "heddle merge should persist an in-progress merge state for continue"
    );
    let second_merge = heddle_output(&["--output", "json", "merge", "main"], Some(temp.path()))
        .expect("invoke merge while merge state exists");
    assert!(
        !second_merge.status.success(),
        "second merge should refuse while merge state exists"
    );
    assert!(
        second_merge.stdout.is_empty(),
        "JSON-mode active-merge refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&second_merge.stdout)
    );
    let stderr = std::str::from_utf8(&second_merge.stderr).unwrap();
    let envelope: serde_json::Value =
        serde_json::from_str(stderr).expect("active merge refusal should emit JSON envelope");
    assert_eq!(envelope["kind"], "merge_already_in_progress");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("merge is already in progress")),
        "active merge refusal should keep full typed advice: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle status")
                && hint.contains("heddle continue")
                && hint.contains("heddle resolve --abort")),
        "active merge refusal should name recovery commands: {stderr}"
    );
    heddle(&["resolve", "--all", "--ours"], Some(temp.path())).unwrap();

    let continued = json(temp.path(), &["--output", "json", "continue"]);
    assert_eq!(continued["status"], "continued");

    let status_after_continue = json(temp.path(), &["status", "--output", "json"]);
    assert!(status_after_continue["operation"].is_null());

    let git_repo = TempDir::new().unwrap();
    init_git_repo_with_branch(git_repo.path(), "feature/drop-in");
    std::fs::write(git_repo.path().join("base.txt"), "base\n").unwrap();
    git_commit_all(git_repo.path(), "seed branch");
    heddle(&["init"], Some(git_repo.path())).unwrap();
    let _ = json(git_repo.path(), &["status", "--output", "json"]);
    git(&["checkout", "-b", "support/rebase"], git_repo.path());
    std::fs::write(git_repo.path().join("clash.txt"), "support rebase\n").unwrap();
    git_commit_all(git_repo.path(), "support rebase");
    git(&["checkout", "feature/drop-in"], git_repo.path());
    std::fs::write(git_repo.path().join("clash.txt"), "main rebase\n").unwrap();
    git_commit_all(git_repo.path(), "main rebase");
    git(&["checkout", "support/rebase"], git_repo.path());
    let rebase = Command::new("git")
        .args(["rebase", "feature/drop-in"])
        .current_dir(git_repo.path())
        .output()
        .expect("git rebase should run");
    assert!(!rebase.status.success());

    let aborted = json(git_repo.path(), &["--output", "json", "abort"]);
    assert_eq!(aborted["status"], "blocked");
    assert_eq!(aborted["recommended_action"], raw_git_preservation_action());
    let status_after_abort = json(git_repo.path(), &["status", "--output", "json"]);
    assert_eq!(status_after_abort["operation"]["kind"], "rebase");
}

#[test]
fn git_overlay_matrix_rebase_noop_defers_up_to_date_claim_to_verification() {
    let remote = TempDir::new().unwrap();
    git(
        &["init", "--bare", remote.path().to_str().unwrap()],
        remote.path(),
    );

    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    git(
        &["remote", "add", "origin", remote.path().to_str().unwrap()],
        temp.path(),
    );
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["push", "-u", "origin", "feature/drop-in"], temp.path());
    heddle_adopt(temp.path());

    let other = TempDir::new().unwrap();
    git(
        &[
            "clone",
            remote.path().to_str().unwrap(),
            other.path().to_str().unwrap(),
        ],
        temp.path(),
    );
    git(&["config", "user.name", "Heddle Test"], other.path());
    git(
        &["config", "user.email", "heddle@example.com"],
        other.path(),
    );
    git(&["checkout", "feature/drop-in"], other.path());
    std::fs::write(other.path().join("tracked.txt"), "remote advanced\n").unwrap();
    git_commit_all(other.path(), "remote advance");
    git(&["push", "origin", "feature/drop-in"], other.path());
    git(&["fetch", "origin"], temp.path());

    let verify_before = json(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(verify_before["verified"], false);
    assert_eq!(verify_before["remote_drift"], "remote_behind");

    let rebase = json(
        temp.path(),
        &["--output", "json", "rebase", "feature/drop-in"],
    );
    assert_eq!(rebase["status"], "blocked");
    assert_eq!(rebase["reason"], "repository_verification");
    assert_eq!(rebase["verification"]["verified"], false);
    assert_eq!(rebase["verification"]["remote_drift"], "remote_behind");
    assert_eq!(rebase["recommended_action"], "heddle pull");
    assert_eq!(
        rebase["recommended_action_template"]["argv_template"],
        heddle_argv_json(["pull"])
    );
}

#[test]
fn git_overlay_matrix_sync_and_primary_guidance_prefer_heddle_verbs() {
    let remote = TempDir::new().unwrap();
    git(
        &["init", "--bare", remote.path().to_str().unwrap()],
        remote.path(),
    );

    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    git(
        &["remote", "add", "origin", remote.path().to_str().unwrap()],
        temp.path(),
    );
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["push", "-u", "origin", "feature/drop-in"], temp.path());
    heddle_adopt(temp.path());

    let other = TempDir::new().unwrap();
    git(
        &[
            "clone",
            remote.path().to_str().unwrap(),
            other.path().to_str().unwrap(),
        ],
        temp.path(),
    );
    // Clone does not inherit user identity from the remote; configure it
    // explicitly so `git commit` succeeds on CI runners without a global
    // git config.
    git(&["config", "user.name", "Heddle Test"], other.path());
    git(
        &["config", "user.email", "heddle@example.com"],
        other.path(),
    );
    git(&["checkout", "feature/drop-in"], other.path());
    std::fs::write(other.path().join("tracked.txt"), "remote advanced\n").unwrap();
    git_commit_all(other.path(), "remote advance");
    git(&["push", "origin", "feature/drop-in"], other.path());
    git(&["fetch", "origin"], temp.path());

    let status_before = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status_before["remote_tracking"]["behind"], 1);
    assert_eq!(status_before["recommended_action"], "heddle pull");

    let diagnose_before = json(temp.path(), &["doctor", "--output", "json"]);
    assert_eq!(
        diagnose_before["health"]["recommended_action"],
        "heddle pull"
    );

    let sync = json(temp.path(), &["--output", "json", "sync"]);
    assert_eq!(sync["status"], "synced");

    let verify_after_sync = json(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(
        verify_after_sync["verified"], true,
        "sync may report synced only after the shared verify engine is clean: {verify_after_sync}"
    );

    let status_after = json(temp.path(), &["status", "--output", "json"]);
    assert!(status_after["remote_tracking"].is_null());
}

#[test]
fn git_overlay_matrix_continue_handles_each_supported_operation_state() {
    // Heddle merge: unresolved conflicts should block, then continue should finish once resolved.
    let heddle_merge = TempDir::new().unwrap();
    init_heddle_conflict_repo(heddle_merge.path());
    start_conflicted_heddle_merge(heddle_merge.path());

    let conflict_list = json(
        heddle_merge.path(),
        &["conflict", "list", "--output", "json"],
    );
    assert_eq!(
        conflict_list["conflicts"][0]["file"], "conflict.txt",
        "conflict list should surface active text-merge conflicts, not only structured conflict blobs: {conflict_list}"
    );
    let conflict_show = json(
        heddle_merge.path(),
        &["conflict", "show", "conflict.txt", "--output", "json"],
    );
    assert_eq!(conflict_show["kind"], "active_merge_conflict");
    assert_eq!(conflict_show["file"], "conflict.txt");
    assert_eq!(
        conflict_show["recommended_action"],
        "heddle resolve conflict.txt"
    );
    assert_eq!(
        conflict_show["recommended_action_template"]["argv_template"],
        heddle_argv_json(["resolve", "conflict.txt"])
    );
    assert_eq!(
        conflict_show["next_action_template"]["argv_template"],
        heddle_argv_json(["resolve", "conflict.txt"])
    );
    assert!(
        conflict_show["worktree_content"]
            .as_str()
            .is_some_and(|content| content.contains("<<<<<<<") && content.contains(">>>>>>>")),
        "conflict show should expose the active conflict-marker content: {conflict_show}"
    );
    let conflict_show_text = heddle(
        &["conflict", "show", "conflict.txt", "--output", "text"],
        Some(heddle_merge.path()),
    )
    .expect("conflict show text should render");
    assert!(
        conflict_show_text.contains("active text merge")
            && conflict_show_text.contains("<<<<<<<")
            && conflict_show_text.contains("next: heddle resolve conflict.txt"),
        "text conflict show should inspect active merge conflicts instead of reporting not found: {conflict_show_text}"
    );

    let blocked_continue = json(heddle_merge.path(), &["--output", "json", "continue"]);
    assert_eq!(blocked_continue["status"], "blocked");
    assert_eq!(blocked_continue["next_action"], "heddle resolve --list");
    assert_eq!(
        blocked_continue["recommended_action"],
        "heddle resolve conflict.txt"
    );

    heddle(&["resolve", "--all", "--ours"], Some(heddle_merge.path())).unwrap();
    let continued_merge = json(heddle_merge.path(), &["--output", "json", "continue"]);
    assert_eq!(continued_merge["status"], "continued");
    assert!(json(heddle_merge.path(), &["status", "--output", "json"])["operation"].is_null());

    // Raw Git merge: Heddle must not shell out to `git merge --continue`.
    let git_merge = TempDir::new().unwrap();
    init_git_repo_with_branch(git_merge.path(), "feature/drop-in");
    std::fs::write(git_merge.path().join("conflict.txt"), "base\n").unwrap();
    git_commit_all(git_merge.path(), "seed branch");
    heddle(&["init"], Some(git_merge.path())).unwrap();
    let _ = json(git_merge.path(), &["status", "--output", "json"]);
    git(&["checkout", "-b", "support/merge"], git_merge.path());
    std::fs::write(git_merge.path().join("conflict.txt"), "support merge\n").unwrap();
    git_commit_all(git_merge.path(), "support merge");
    git(&["checkout", "feature/drop-in"], git_merge.path());
    std::fs::write(git_merge.path().join("conflict.txt"), "main merge\n").unwrap();
    git_commit_all(git_merge.path(), "main merge");
    let merge = Command::new("git")
        .args(["merge", "support/merge"])
        .current_dir(git_merge.path())
        .output()
        .expect("git merge should run");
    assert!(!merge.status.success());
    std::fs::write(git_merge.path().join("conflict.txt"), "main merge\n").unwrap();
    git(&["add", "conflict.txt"], git_merge.path());
    let continued_git_merge = json(git_merge.path(), &["--output", "json", "continue"]);
    assert_eq!(continued_git_merge["status"], "blocked");
    assert_eq!(
        continued_git_merge["recommended_action"],
        "heddle bridge git status"
    );
    assert!(
        continued_git_merge["message"]
            .as_str()
            .is_some_and(|message| message.contains("no-git runtime")),
        "raw Git handoff should explain why Heddle did not run git: {continued_git_merge}"
    );
    assert!(!json(git_merge.path(), &["status", "--output", "json"])["operation"].is_null());

    // Raw Git cherry-pick: Heddle must not shell out to `git cherry-pick --continue`.
    let git_cherry = TempDir::new().unwrap();
    init_git_repo_with_branch(git_cherry.path(), "feature/drop-in");
    std::fs::write(git_cherry.path().join("conflict.txt"), "base\n").unwrap();
    git_commit_all(git_cherry.path(), "seed branch");
    let _ = json(git_cherry.path(), &["status", "--output", "json"]);
    git(&["checkout", "-b", "support/cherry"], git_cherry.path());
    std::fs::write(git_cherry.path().join("conflict.txt"), "support cherry\n").unwrap();
    git_commit_all(git_cherry.path(), "support cherry");
    let cherry_commit = {
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(git_cherry.path())
            .output()
            .expect("git rev-parse should run");
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    git(&["checkout", "feature/drop-in"], git_cherry.path());
    std::fs::write(git_cherry.path().join("conflict.txt"), "main cherry\n").unwrap();
    git_commit_all(git_cherry.path(), "main cherry");
    let cherry_pick = Command::new("git")
        .args(["cherry-pick", &cherry_commit])
        .current_dir(git_cherry.path())
        .output()
        .expect("git cherry-pick should run");
    assert!(!cherry_pick.status.success());
    std::fs::write(git_cherry.path().join("conflict.txt"), "main cherry\n").unwrap();
    git(&["add", "conflict.txt"], git_cherry.path());
    let continued_git_cherry = json(git_cherry.path(), &["--output", "json", "continue"]);
    assert_eq!(continued_git_cherry["status"], "blocked");
    assert_eq!(
        continued_git_cherry["recommended_action"],
        "heddle bridge git status"
    );
    assert!(!json(git_cherry.path(), &["status", "--output", "json"])["operation"].is_null());

    // Raw Git revert: Heddle must not shell out to `git revert --continue`.
    let git_revert = TempDir::new().unwrap();
    init_git_repo_with_branch(git_revert.path(), "feature/drop-in");
    std::fs::write(git_revert.path().join("tracked.txt"), "base\n").unwrap();
    git_commit_all(git_revert.path(), "seed branch");
    let _ = json(git_revert.path(), &["status", "--output", "json"]);
    std::fs::write(git_revert.path().join("tracked.txt"), "main change\n").unwrap();
    git_commit_all(git_revert.path(), "main change");
    let revert = Command::new("git")
        .args(["revert", "--no-commit", "HEAD"])
        .current_dir(git_revert.path())
        .output()
        .expect("git revert should run");
    assert!(revert.status.success());
    git(&["add", "tracked.txt"], git_revert.path());
    let continued_git_revert = json(git_revert.path(), &["--output", "json", "continue"]);
    assert_eq!(continued_git_revert["status"], "blocked");
    assert_eq!(
        continued_git_revert["recommended_action"],
        "heddle bridge git status"
    );
    assert!(!json(git_revert.path(), &["status", "--output", "json"])["operation"].is_null());

    // Bisect states should remain intentionally blocked under continue.
    let heddle_bisect = TempDir::new().unwrap();
    init_git_repo_with_branch(heddle_bisect.path(), "feature/drop-in");
    std::fs::write(heddle_bisect.path().join("tracked.txt"), "base\n").unwrap();
    git_commit_all(heddle_bisect.path(), "seed branch");
    let _ = json(heddle_bisect.path(), &["status", "--output", "json"]);
    seed_heddle_bisect_state(heddle_bisect.path());
    let blocked_heddle_bisect = json(heddle_bisect.path(), &["--output", "json", "continue"]);
    assert_eq!(blocked_heddle_bisect["status"], "blocked");
    assert_eq!(
        blocked_heddle_bisect["recommended_action"],
        "heddle abort"
    );

    let git_bisect = TempDir::new().unwrap();
    init_git_repo_with_branch(git_bisect.path(), "feature/drop-in");
    std::fs::write(git_bisect.path().join("tracked.txt"), "base\n").unwrap();
    git_commit_all(git_bisect.path(), "seed branch");
    std::fs::write(git_bisect.path().join("tracked.txt"), "middle\n").unwrap();
    git_commit_all(git_bisect.path(), "middle change");
    std::fs::write(git_bisect.path().join("tracked.txt"), "bad\n").unwrap();
    git_commit_all(git_bisect.path(), "bad change");
    let _ = json(git_bisect.path(), &["status", "--output", "json"]);
    git(&["bisect", "start"], git_bisect.path());
    git(&["bisect", "bad"], git_bisect.path());
    git(&["bisect", "good", "HEAD~2"], git_bisect.path());
    let blocked_git_bisect = json(git_bisect.path(), &["--output", "json", "continue"]);
    assert_eq!(blocked_git_bisect["status"], "blocked");
    assert_eq!(
        blocked_git_bisect["recommended_action"],
        "heddle bridge git status"
    );
}

#[test]
fn git_overlay_matrix_abort_handles_each_supported_operation_state() {
    let heddle_merge = TempDir::new().unwrap();
    init_heddle_conflict_repo(heddle_merge.path());
    start_conflicted_heddle_merge(heddle_merge.path());
    let aborted_heddle_merge = json(heddle_merge.path(), &["--output", "json", "abort"]);
    assert_eq!(aborted_heddle_merge["status"], "aborted");
    assert!(json(heddle_merge.path(), &["status", "--output", "json"])["operation"].is_null());

    let heddle_bisect = TempDir::new().unwrap();
    init_git_repo_with_branch(heddle_bisect.path(), "feature/drop-in");
    std::fs::write(heddle_bisect.path().join("tracked.txt"), "base\n").unwrap();
    git_commit_all(heddle_bisect.path(), "seed branch");
    let _ = json(heddle_bisect.path(), &["status", "--output", "json"]);
    seed_heddle_bisect_state(heddle_bisect.path());
    let aborted_heddle_bisect = json(heddle_bisect.path(), &["--output", "json", "abort"]);
    assert_eq!(aborted_heddle_bisect["status"], "aborted");
    assert!(json(heddle_bisect.path(), &["status", "--output", "json"])["operation"].is_null());

    let git_rebase = TempDir::new().unwrap();
    init_git_repo_with_branch(git_rebase.path(), "feature/drop-in");
    std::fs::write(git_rebase.path().join("clash.txt"), "base\n").unwrap();
    git_commit_all(git_rebase.path(), "seed branch");
    let _ = json(git_rebase.path(), &["status", "--output", "json"]);
    git(&["checkout", "-b", "support/rebase"], git_rebase.path());
    std::fs::write(git_rebase.path().join("clash.txt"), "support rebase\n").unwrap();
    git_commit_all(git_rebase.path(), "support rebase");
    git(&["checkout", "feature/drop-in"], git_rebase.path());
    std::fs::write(git_rebase.path().join("clash.txt"), "main rebase\n").unwrap();
    git_commit_all(git_rebase.path(), "main rebase");
    git(&["checkout", "support/rebase"], git_rebase.path());
    let rebase = Command::new("git")
        .args(["rebase", "feature/drop-in"])
        .current_dir(git_rebase.path())
        .output()
        .expect("git rebase should run");
    assert!(!rebase.status.success());
    let aborted_git_rebase = json(git_rebase.path(), &["--output", "json", "abort"]);
    assert_eq!(aborted_git_rebase["status"], "blocked");
    assert_eq!(
        aborted_git_rebase["recommended_action"],
        "heddle bridge git status"
    );
    assert!(!json(git_rebase.path(), &["status", "--output", "json"])["operation"].is_null());

    let git_merge = TempDir::new().unwrap();
    init_git_repo_with_branch(git_merge.path(), "feature/drop-in");
    std::fs::write(git_merge.path().join("conflict.txt"), "base\n").unwrap();
    git_commit_all(git_merge.path(), "seed branch");
    heddle(&["init"], Some(git_merge.path())).unwrap();
    let _ = json(git_merge.path(), &["status", "--output", "json"]);
    git(&["checkout", "-b", "support/merge"], git_merge.path());
    std::fs::write(git_merge.path().join("conflict.txt"), "support merge\n").unwrap();
    git_commit_all(git_merge.path(), "support merge");
    git(&["checkout", "feature/drop-in"], git_merge.path());
    std::fs::write(git_merge.path().join("conflict.txt"), "main merge\n").unwrap();
    git_commit_all(git_merge.path(), "main merge");
    let merge = Command::new("git")
        .args(["merge", "support/merge"])
        .current_dir(git_merge.path())
        .output()
        .expect("git merge should run");
    assert!(!merge.status.success());
    let aborted_git_merge = json(git_merge.path(), &["--output", "json", "abort"]);
    assert_eq!(aborted_git_merge["status"], "blocked");
    assert_eq!(
        aborted_git_merge["recommended_action"],
        "heddle bridge git status"
    );
    assert!(!json(git_merge.path(), &["status", "--output", "json"])["operation"].is_null());

    let git_cherry = TempDir::new().unwrap();
    init_git_repo_with_branch(git_cherry.path(), "feature/drop-in");
    std::fs::write(git_cherry.path().join("conflict.txt"), "base\n").unwrap();
    git_commit_all(git_cherry.path(), "seed branch");
    let _ = json(git_cherry.path(), &["status", "--output", "json"]);
    git(&["checkout", "-b", "support/cherry"], git_cherry.path());
    std::fs::write(git_cherry.path().join("conflict.txt"), "support cherry\n").unwrap();
    git_commit_all(git_cherry.path(), "support cherry");
    let cherry_commit = {
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(git_cherry.path())
            .output()
            .expect("git rev-parse should run");
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    git(&["checkout", "feature/drop-in"], git_cherry.path());
    std::fs::write(git_cherry.path().join("conflict.txt"), "main cherry\n").unwrap();
    git_commit_all(git_cherry.path(), "main cherry");
    let cherry_pick = Command::new("git")
        .args(["cherry-pick", &cherry_commit])
        .current_dir(git_cherry.path())
        .output()
        .expect("git cherry-pick should run");
    assert!(!cherry_pick.status.success());
    let aborted_git_cherry = json(git_cherry.path(), &["--output", "json", "abort"]);
    assert_eq!(aborted_git_cherry["status"], "blocked");
    assert_eq!(
        aborted_git_cherry["recommended_action"],
        "heddle bridge git status"
    );
    assert!(!json(git_cherry.path(), &["status", "--output", "json"])["operation"].is_null());

    let git_revert = TempDir::new().unwrap();
    init_git_repo_with_branch(git_revert.path(), "feature/drop-in");
    std::fs::write(git_revert.path().join("tracked.txt"), "base\n").unwrap();
    git_commit_all(git_revert.path(), "seed branch");
    let _ = json(git_revert.path(), &["status", "--output", "json"]);
    std::fs::write(git_revert.path().join("tracked.txt"), "main change\n").unwrap();
    git_commit_all(git_revert.path(), "main change");
    let revert = Command::new("git")
        .args(["revert", "--no-commit", "HEAD"])
        .current_dir(git_revert.path())
        .output()
        .expect("git revert should run");
    assert!(revert.status.success());
    let aborted_git_revert = json(git_revert.path(), &["--output", "json", "abort"]);
    assert_eq!(aborted_git_revert["status"], "blocked");
    assert_eq!(
        aborted_git_revert["recommended_action"],
        "heddle bridge git status"
    );
    assert!(!json(git_revert.path(), &["status", "--output", "json"])["operation"].is_null());

    let git_bisect = TempDir::new().unwrap();
    init_git_repo_with_branch(git_bisect.path(), "feature/drop-in");
    std::fs::write(git_bisect.path().join("tracked.txt"), "base\n").unwrap();
    git_commit_all(git_bisect.path(), "seed branch");
    std::fs::write(git_bisect.path().join("tracked.txt"), "middle\n").unwrap();
    git_commit_all(git_bisect.path(), "middle change");
    std::fs::write(git_bisect.path().join("tracked.txt"), "bad\n").unwrap();
    git_commit_all(git_bisect.path(), "bad change");
    let _ = json(git_bisect.path(), &["status", "--output", "json"]);
    git(&["bisect", "start"], git_bisect.path());
    git(&["bisect", "bad"], git_bisect.path());
    git(&["bisect", "good", "HEAD~2"], git_bisect.path());
    let aborted_git_bisect = json(git_bisect.path(), &["--output", "json", "abort"]);
    assert_eq!(aborted_git_bisect["status"], "blocked");
    assert_eq!(
        aborted_git_bisect["recommended_action"],
        "heddle bridge git status"
    );
    assert!(!json(git_bisect.path(), &["status", "--output", "json"])["operation"].is_null());
}

#[test]
fn git_overlay_matrix_operator_states_survive_reopen_and_keep_guidance_consistent() {
    let temp = TempDir::new().unwrap();
    init_heddle_conflict_repo(temp.path());
    start_conflicted_heddle_merge(temp.path());

    let status = json(temp.path(), &["status", "--output", "json"]);
    let diagnose = json(temp.path(), &["doctor", "--output", "json"]);
    let thread_show = json(
        temp.path(),
        &["thread", "show", "feature", "--output", "json"],
    );
    let workspace = json(temp.path(), &["workspace", "show", "--output", "json"]);

    assert_eq!(status["operation"]["kind"], "merge");
    assert_eq!(diagnose["operation"]["kind"], "merge");
    assert_eq!(thread_show["operation"]["kind"], "merge");
    assert_eq!(workspace["operation"]["kind"], "merge");
    assert_eq!(status["recommended_action"], "heddle continue");
    assert_eq!(diagnose["health"]["recommended_action"], "heddle continue");
    assert_eq!(thread_show["recommended_action"], "heddle continue");
    assert_eq!(workspace["recommended_action"], "heddle continue");

    let nested = temp.path().join("nested/reopen/path");
    std::fs::create_dir_all(&nested).unwrap();
    let status_reopened = json(&nested, &["status", "--output", "json"]);
    let workspace_reopened = json(&nested, &["workspace", "show", "--output", "json"]);
    assert_eq!(status_reopened["operation"]["kind"], "merge");
    assert_eq!(status_reopened["recommended_action"], "heddle continue");
    assert_eq!(workspace_reopened["recommended_action"], "heddle continue");
}

#[test]
fn git_overlay_matrix_continue_retry_loops_block_then_succeed_after_resolution() {
    let heddle_merge = TempDir::new().unwrap();
    init_heddle_conflict_repo(heddle_merge.path());
    start_conflicted_heddle_merge(heddle_merge.path());
    let blocked = json(heddle_merge.path(), &["--output", "json", "continue"]);
    assert_eq!(blocked["status"], "blocked");
    assert_operator_json_contract(&blocked, "continue");
    heddle(&["resolve", "--all", "--ours"], Some(heddle_merge.path())).unwrap();
    let continued = json(heddle_merge.path(), &["--output", "json", "continue"]);
    assert_eq!(continued["status"], "continued");
    assert_operator_json_contract(&continued, "merge");

    let git_merge = TempDir::new().unwrap();
    init_git_repo_with_branch(git_merge.path(), "feature/drop-in");
    std::fs::write(git_merge.path().join("conflict.txt"), "base\n").unwrap();
    git_commit_all(git_merge.path(), "seed branch");
    heddle(&["init"], Some(git_merge.path())).unwrap();
    let _ = json(git_merge.path(), &["status", "--output", "json"]);
    git(&["checkout", "-b", "support/merge"], git_merge.path());
    std::fs::write(git_merge.path().join("conflict.txt"), "support merge\n").unwrap();
    git_commit_all(git_merge.path(), "support merge");
    git(&["checkout", "feature/drop-in"], git_merge.path());
    std::fs::write(git_merge.path().join("conflict.txt"), "main merge\n").unwrap();
    git_commit_all(git_merge.path(), "main merge");
    let merge = Command::new("git")
        .args(["merge", "support/merge"])
        .current_dir(git_merge.path())
        .output()
        .expect("git merge should run");
    assert!(!merge.status.success());
    let blocked_git = json(git_merge.path(), &["status", "--output", "json"]);
    assert_eq!(blocked_git["operation"]["kind"], "merge", "{blocked_git}");
    std::fs::write(git_merge.path().join("conflict.txt"), "main merge\n").unwrap();
    git(&["add", "conflict.txt"], git_merge.path());
    let continued_git = json(git_merge.path(), &["--output", "json", "continue"]);
    assert_eq!(continued_git["status"], "blocked");
    assert_eq!(
        continued_git["recommended_action"],
        raw_git_preservation_action()
    );
    assert!(
        continued_git["message"]
            .as_str()
            .is_some_and(|message| message.contains("no-git runtime")),
        "raw Git continue should explain the native no-git boundary: {continued_git}"
    );
    assert_operator_json_contract(&continued_git, "merge");
}

// ---------------------------------------------------------------------------
// `--no-thread` lane-existence conformance (heddle#307, Codex cid 3327525677
// + cid 3327725478).
//
// The bug class: every actor-surface site that asks "is there a current lane /
// active actor for THIS checkout?" must consult the single git-overlay-aware
// oracle `Repository::current_lane()`. In a git-overlay repo, Git HEAD can be
// detached while `.heddle/HEAD` still names a stale attached thread; any site
// that reads `head_ref()` / `.heddle/HEAD` directly then falls back to that
// stale `Attached`, so the execute path attached the actor to a dead branch and
// the recommend path either advertised a `--no-thread` command that cannot
// succeed or resolved a stale actor instead of recommending a mint.
//
// Round 2 routed `actor spawn --no-thread` (execute) and the recommend fallback
// through `current_lane()`. Round 3 (cid 3327725478) closes the *upstream*
// leak: `resolve_actor_entry` decided "is there an attached actor?" off
// `head_ref()` and the unconditional "any active actor" fallback, so an active
// actor on `main` plus a detached-unmapped HEAD resolved the stale `main` actor
// and `actor explain` printed it — never reaching the mint recommendation —
// while `actor spawn --no-thread` rejected. This harness pins the invariant
// across the whole matrix: recommend and execute must agree, including when an
// active actor exists.

/// `actor explain` recommends `--no-thread` iff a current lane exists.
fn explain_recommends_no_thread(path: &std::path::Path) -> bool {
    let output = heddle_output_with_env(
        &["actor", "explain", "--output", "json"],
        Some(path),
        &[
            ("CODEX_THREAD_ID", "thread-cold-agent"),
            ("CODEX_MODEL", "gpt-5.3-codex"),
            ("CODEX_REASONING_EFFORT", "high"),
        ],
    )
    .expect("actor explain should run");
    assert!(
        output.status.success(),
        "actor explain should succeed; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("actor explain JSON should parse: {err}: {stdout}"));
    parsed["recommended_action"]
        .as_str()
        .expect("recommended_action should be a string")
        .contains("--no-thread")
}

/// `actor spawn --no-thread` succeeds iff a current lane exists.
fn spawn_no_thread_succeeds(path: &std::path::Path) -> bool {
    heddle(
        &[
            "actor",
            "spawn",
            "--no-thread",
            "--provider",
            "openai",
            "--model",
            "gpt-5.3-codex",
        ],
        Some(path),
    )
    .is_ok()
}

#[test]
fn git_overlay_no_thread_lane_predicate_recommend_and_execute_agree() {
    // (c) On-lane: adopted git-overlay repo sits attached to `main`. A lane
    // exists, so recommend and execute both accept `--no-thread`.
    let on_lane = TempDir::new().unwrap();
    init_git_repo_with_branch(on_lane.path(), "main");
    std::fs::write(on_lane.path().join("base.txt"), "base\n").unwrap();
    git_commit_all(on_lane.path(), "base");
    heddle_adopt(on_lane.path());
    assert_eq!(
        Repository::open(on_lane.path())
            .unwrap()
            .current_lane()
            .unwrap(),
        Some("main".to_string()),
        "adopted git-overlay repo should report `main` as the current lane"
    );
    assert!(
        explain_recommends_no_thread(on_lane.path()),
        "on-lane: explain should recommend --no-thread"
    );
    assert!(
        spawn_no_thread_succeeds(on_lane.path()),
        "on-lane: spawn --no-thread should succeed"
    );

    // (a) Detached Git HEAD whose commit has NO Heddle mapping. We adopt, then
    // detach and make a *fresh* Git commit directly (bypassing heddle) so the
    // detached commit is provably unmapped. `read_head_state` defaults to
    // `Attached{main}` when `.heddle/HEAD` is absent, so the old `head_ref()`
    // fell back to that stale `main` thread — but there is no lane to attach to.
    let detached_no_mapping = TempDir::new().unwrap();
    init_git_repo_with_branch(detached_no_mapping.path(), "main");
    std::fs::write(detached_no_mapping.path().join("base.txt"), "base\n").unwrap();
    git_commit_all(detached_no_mapping.path(), "base");
    heddle_adopt(detached_no_mapping.path());
    git(&["checkout", "--detach"], detached_no_mapping.path());
    git(
        &["commit", "--allow-empty", "-m", "unmapped detached commit"],
        detached_no_mapping.path(),
    );
    assert_eq!(
        Repository::open(detached_no_mapping.path())
            .unwrap()
            .current_lane()
            .unwrap(),
        None,
        "detached Git HEAD with no Heddle mapping must report no current lane, \
         not the stale `.heddle/HEAD` thread"
    );
    assert!(
        !explain_recommends_no_thread(detached_no_mapping.path()),
        "detached-no-mapping: explain must NOT recommend --no-thread (mint instead)"
    );
    assert!(
        !spawn_no_thread_succeeds(detached_no_mapping.path()),
        "detached-no-mapping: spawn --no-thread must be rejected, not attached to a stale branch"
    );

    // (b) Detached Git HEAD whose commit DOES map to a Heddle change (the
    // adopted tip). It is still detached — no attached lane — so recommend and
    // execute must agree on rejecting `--no-thread`.
    let detached_mapped = TempDir::new().unwrap();
    init_git_repo_with_branch(detached_mapped.path(), "main");
    std::fs::write(detached_mapped.path().join("base.txt"), "base\n").unwrap();
    git_commit_all(detached_mapped.path(), "base");
    std::fs::write(detached_mapped.path().join("next.txt"), "next\n").unwrap();
    git_commit_all(detached_mapped.path(), "next");
    heddle_adopt(detached_mapped.path());
    let mapped_tip = git_stdout(detached_mapped.path(), &["rev-parse", "HEAD"]);
    git(&["checkout", &mapped_tip], detached_mapped.path());
    let recommend = explain_recommends_no_thread(detached_mapped.path());
    let execute = spawn_no_thread_succeeds(detached_mapped.path());
    assert_eq!(
        recommend, execute,
        "detached-with-mapping: recommend and execute must agree on --no-thread"
    );
    assert!(
        !execute,
        "detached-with-mapping: a detached HEAD has no attached lane, so --no-thread is rejected"
    );

    // (d) The leak round 2 missed (cid 3327725478): an *active actor* attached
    // to `main`, then Git HEAD detached to an UNMAPPED commit so `.heddle/HEAD`
    // still says `Attached(main)`. `resolve_actor_entry` used to resolve that
    // stale `main` actor off `head_ref()` (and the unconditional "any active
    // actor" fallback), so `actor explain` printed the stale actor — never
    // reaching the mint recommendation — while `actor spawn --no-thread`
    // rejected. The single `current_lane()` oracle reports no lane here, so
    // explain must fall through to the minting recommendation and agree with
    // execute's rejection.
    let active_then_detached = TempDir::new().unwrap();
    init_git_repo_with_branch(active_then_detached.path(), "main");
    std::fs::write(active_then_detached.path().join("base.txt"), "base\n").unwrap();
    git_commit_all(active_then_detached.path(), "base");
    heddle_adopt(active_then_detached.path());
    // Spawn an actor attached to the current lane (`main`). It records
    // `path: None`, so it is reachable only via the lane oracle / the "any
    // active actor" fallback — exactly the surfaces under test.
    heddle(
        &[
            "actor",
            "spawn",
            "--no-thread",
            "--provider",
            "openai",
            "--model",
            "gpt-5.3-codex",
        ],
        Some(active_then_detached.path()),
    )
    .expect("spawn an active actor attached to `main`");
    git(&["checkout", "--detach"], active_then_detached.path());
    git(
        &["commit", "--allow-empty", "-m", "unmapped detached commit"],
        active_then_detached.path(),
    );
    assert_eq!(
        Repository::open(active_then_detached.path())
            .unwrap()
            .current_lane()
            .unwrap(),
        None,
        "active-actor-on-main + detached-unmapped HEAD must report no current lane"
    );
    // Recommend: `actor explain` must NOT resolve the stale `main` actor. It
    // must report no active actor (`attached: false`) and recommend the minting
    // spawn form, not `--no-thread`.
    let explained = heddle_output_with_env(
        &["actor", "explain", "--output", "json"],
        Some(active_then_detached.path()),
        &[
            ("CODEX_THREAD_ID", "thread-cold-agent"),
            ("CODEX_MODEL", "gpt-5.3-codex"),
            ("CODEX_REASONING_EFFORT", "high"),
        ],
    )
    .expect("actor explain should run");
    assert!(
        explained.status.success(),
        "actor explain should succeed; stderr={}",
        String::from_utf8_lossy(&explained.stderr)
    );
    let explained_json: Value = serde_json::from_slice(&explained.stdout)
        .unwrap_or_else(|err| panic!("actor explain JSON should parse: {err}"));
    assert_eq!(
        explained_json["attached"], false,
        "detached-unmapped HEAD with a stale `.heddle/HEAD` must not resolve the \
         stale `main` actor: {explained_json}"
    );
    let recommended = explained_json["recommended_action"]
        .as_str()
        .unwrap_or_else(|| panic!("recommended_action should be present: {explained_json}"));
    assert!(
        recommended.contains("actor spawn") && !recommended.contains("--no-thread"),
        "active-actor-on-main + detached-unmapped: explain must recommend the minting \
         spawn form, not `--no-thread`: {explained_json}"
    );
    // Execute agrees: `--no-thread` is rejected because there is no current lane.
    assert!(
        !spawn_no_thread_succeeds(active_then_detached.path()),
        "active-actor-on-main + detached-unmapped: spawn --no-thread must be rejected"
    );
}
