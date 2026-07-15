// SPDX-License-Identifier: Apache-2.0
use super::{git_overlay_fixtures::GitOverlayFixture, *};

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
    std::fs::write(path.join(".heddle").join("BISECT_STATE"), "{}\n").expect("seed BISECT_STATE");
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

fn directory_bytes_snapshot(root: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    fn visit(root: &std::path::Path, path: &std::path::Path, out: &mut Vec<(String, Vec<u8>)>) {
        let mut entries = std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        entries.sort();
        for entry in entries {
            if entry.is_dir() {
                visit(root, &entry, out);
            } else {
                out.push((
                    entry.strip_prefix(root).unwrap().display().to_string(),
                    std::fs::read(&entry).unwrap(),
                ));
            }
        }
    }
    let mut snapshot = Vec::new();
    visit(root, root, &mut snapshot);
    snapshot
}

fn git_commit_all(path: &std::path::Path, message: &str) {
    git(&["add", "."], path);
    git(&["commit", "-m", message], path);
}

fn initialize_git_overlay(path: &std::path::Path) {
    heddle(&["init"], Some(path)).unwrap();
}

#[test]
fn initialized_overlay_observe_commands_project_full_git_history_without_writes() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("story.txt"), "one\n").unwrap();
    git_commit_all(temp.path(), "first");
    git(&["config", "user.name", "Second Author"], temp.path());
    std::fs::write(temp.path().join("story.txt"), "one\ntwo\n").unwrap();
    git_commit_all(temp.path(), "second");
    initialize_git_overlay(temp.path());
    std::fs::write(temp.path().join("dirty.txt"), "visible diff\n").unwrap();

    let before_files = directory_bytes_snapshot(&temp.path().join(".heddle"));
    let before_refs = git_ref_snapshot(temp.path());

    let log: Value =
        serde_json::from_str(&heddle(&["log", "--output", "json"], Some(temp.path())).unwrap())
            .unwrap();
    assert!(
        log["states"]
            .as_array()
            .is_some_and(|states| states.len() == 2)
    );
    assert!(
        log["states"][0]["parents"]
            .as_array()
            .is_some_and(|parents| !parents.is_empty()),
        "lazy log must retain Git ancestry: {log}"
    );
    let show: Value = serde_json::from_str(
        &heddle(&["show", "HEAD", "--output", "json"], Some(temp.path())).unwrap(),
    )
    .unwrap();
    assert!(
        show["parents"]
            .as_array()
            .is_some_and(|parents| !parents.is_empty())
    );
    let blame: Value = serde_json::from_str(
        &heddle(
            &["query", "--attribution", "story.txt", "--output", "json"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(blame["lines"].as_array().map(Vec::len), Some(2));
    assert_eq!(blame["lines"][0]["principal"]["name"], "Heddle Test");
    assert_eq!(blame["lines"][1]["principal"]["name"], "Second Author");
    heddle(&["diff", "HEAD", "--output", "json"], Some(temp.path())).unwrap();

    assert_eq!(
        directory_bytes_snapshot(&temp.path().join(".heddle")),
        before_files
    );
    assert_eq!(git_ref_snapshot(temp.path()), before_refs);
}

#[test]
fn unbound_overlay_history_uses_native_query_and_canonical_revision_semantics() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    git_commit_all(temp.path(), "base");
    let base_oid = git_stdout(temp.path(), &["rev-parse", "HEAD"]);
    git(&["tag", "base-tag", &base_oid], temp.path());

    git(&["checkout", "-b", "side"], temp.path());
    git(&["config", "user.name", "Side Author"], temp.path());
    std::fs::write(temp.path().join("side.txt"), "side\n").unwrap();
    git_commit_all(temp.path(), "side change");

    git(&["checkout", "main"], temp.path());
    git(
        &["config", "user.name", "principal-agent-model-hit"],
        temp.path(),
    );
    std::fs::write(temp.path().join("main.txt"), "main\n").unwrap();
    git_commit_all(temp.path(), "main change");
    git(
        &["merge", "--no-ff", "side", "-m", "merge side"],
        temp.path(),
    );
    initialize_git_overlay(temp.path());

    let before_files = directory_bytes_snapshot(&temp.path().join(".heddle"));
    let first_parent = json(temp.path(), &["log", "-n", "10"]);
    let intents = first_parent["states"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|state| state["intent"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(intents, vec!["merge side", "main change", "base"]);

    let zero = json(temp.path(), &["log", "-n", "0"]);
    assert_eq!(zero["states"].as_array().map(Vec::len), Some(0));

    let path_log = json(temp.path(), &["log", "-n", "10", "--path", "main.txt"]);
    let path_intents = path_log["states"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|state| state["intent"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(path_intents, vec!["main change"]);

    let principal_is_not_agent = json(
        temp.path(),
        &["log", "--agent", "agent-model-hit", "-n", "10"],
    );
    assert_eq!(
        principal_is_not_agent["states"].as_array().map(Vec::len),
        Some(0)
    );

    let head_parent = json(temp.path(), &["show", "HEAD~1"]);
    assert_eq!(head_parent["intent"], "main change");
    let at_grandparent = json(temp.path(), &["show", "@~2"]);
    assert_eq!(at_grandparent["intent"], "base");
    let branch = json(temp.path(), &["show", "side"]);
    assert_eq!(branch["intent"], "side change");
    let full_tag = json(temp.path(), &["show", "refs/tags/base-tag"]);
    assert_eq!(full_tag["intent"], "base");

    assert_eq!(
        directory_bytes_snapshot(&temp.path().join(".heddle")),
        before_files,
        "unbound query and canonical revision reads must not persist Heddle state"
    );
}

fn raw_git_preservation_action() -> &'static str {
    "heddle verify"
}

fn verify_state_for_assertions(value: Value) -> Value {
    let Some(verification) = value.get("verification") else {
        return value;
    };
    let mut state = verification.clone();
    if let Some(object) = state.as_object_mut() {
        object
            .entry("output_kind".to_string())
            .or_insert_with(|| Value::String("verify".to_string()));
        if let Some(clean) = value.get("clean") {
            object
                .entry("clean".to_string())
                .or_insert_with(|| clean.clone());
        }
    }
    state
}

fn assert_no_legacy_verification_sidecars(value: &Value) {
    for legacy in ["git_overlay_import_hint", "git_overlay_health"] {
        assert!(
            value.get(legacy).is_none(),
            "JSON output must not expose legacy verification sidecar `{legacy}`: {value}"
        );
    }
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
        if args.contains(&"verify") {
            return verify_state_for_assertions(parsed);
        }
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

fn json_success_with_env(cwd: &std::path::Path, args: &[&str], env: &[(&str, &str)]) -> Value {
    let output = heddle_output_with_env(args, Some(cwd), env)
        .unwrap_or_else(|err| panic!("heddle {args:?}: {err}"));
    assert!(
        output.status.success(),
        "heddle {args:?} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|err| panic!("expected JSON for {args:?}: {err}"))
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
    } else if let Some(verification) = parsed.get("verification") {
        verification.clone()
    } else {
        let mut obj_map = parsed.as_object().cloned().unwrap_or_default();
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

#[test]
fn git_overlay_matrix_undo_reconciles_stale_mirror_after_push_pull() {
    let temp = TempDir::new().unwrap();
    let work = temp.path().join("work");
    let origin = temp.path().join("origin.git");
    std::fs::create_dir(&work).unwrap();
    init_git_repo_with_branch(&work, "main");
    std::fs::write(work.join("base.txt"), "base\n").unwrap();
    git_commit_all(&work, "base");
    git(
        &[
            "clone",
            "--bare",
            work.to_str().unwrap(),
            origin.to_str().unwrap(),
        ],
        temp.path(),
    );

    initialize_git_overlay(&work);
    json(&work, &["import", "git", "--ref", "main"]);
    std::fs::write(work.join("first.txt"), "first\n").unwrap();
    json(&work, &["capture", "-m", "first"]);
    json(&work, &["commit", "-m", "first"]);
    json(&work, &["push", origin.to_str().unwrap()]);
    json(&work, &["pull", "origin"]);

    let previous = git_stdout(&work, &["rev-parse", "HEAD"]);
    std::fs::write(work.join("second.txt"), "second\n").unwrap();
    json(&work, &["capture", "-m", "second"]);
    json(&work, &["commit", "-m", "second"]);
    let committed = git_stdout(&work, &["rev-parse", "HEAD"]);
    assert_ne!(committed, previous);

    let undo = json(&work, &["undo"]);
    assert_eq!(undo["status"], "completed", "{undo}");
    assert_eq!(git_stdout(&work, &["rev-parse", "HEAD"]), previous);
    let mirror = work.join(".heddle/git");
    assert_eq!(
        git_stdout(
            &work,
            &[
                "--git-dir",
                mirror.to_str().unwrap(),
                "rev-parse",
                "refs/heads/main",
            ],
        ),
        previous
    );
}

#[test]
fn git_overlay_matrix_commit_prefers_heddle_principal_over_git_identity() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(&["config", "user.name", "Repo Local"], temp.path());
    git(&["config", "user.email", "local@example.com"], temp.path());

    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("local.txt"), "local\n").unwrap();
    let env = [
        ("HEDDLE_PRINCIPAL_NAME", "Heddle Principal"),
        ("HEDDLE_PRINCIPAL_EMAIL", "principal@example.com"),
    ];
    let capture = json_success_with_env(
        temp.path(),
        &[
            "--output",
            "json",
            "capture",
            "-m",
            "local identity capture",
        ],
        &env,
    );
    json_success_with_env(
        temp.path(),
        &["--output", "json", "commit", "-m", "local identity commit"],
        &env,
    );

    assert_eq!(capture["principal"]["name"], "Heddle Principal");
    assert_eq!(capture["principal"]["email"], "principal@example.com");
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
    let capture = json(temp.path(), &["capture", "-m", "local identity capture"]);
    json(temp.path(), &["commit", "-m", "local identity commit"]);

    assert_eq!(capture["principal"]["name"], "Repo Local");
    assert_eq!(capture["principal"]["email"], "local@example.com");
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
    let capture = json(temp.path(), &["capture", "-m", "local identity capture"]);
    json(temp.path(), &["commit", "-m", "local identity commit"]);

    assert_eq!(capture["principal"]["name"], "Repo Local");
    assert_eq!(capture["principal"]["email"], "local@example.com");
    let identity = git_stdout(temp.path(), &["log", "-1", "--format=%an <%ae>%n%cn <%ce>"]);
    assert_eq!(
        identity,
        "Repo Local <local@example.com>\nRepo Local <local@example.com>"
    );
}

#[test]
fn git_overlay_matrix_isolated_checkout_uses_native_capture_with_parent_git_identity() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    git(&["config", "user.name", "Audit User"], temp.path());
    git(&["config", "user.email", "audit@example.com"], temp.path());
    std::fs::write(temp.path().join("README.md"), "base\n").unwrap();
    git_commit_all(temp.path(), "base");
    initialize_git_overlay(temp.path());

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

    let status = json(&checkout, &["status"]);
    assert_eq!(status["repository_capability"], "native-heddle");
    assert_eq!(status["storage_model"], "heddle-native");

    std::fs::write(checkout.join("audit.txt"), "isolated\n").unwrap();
    let env = [("HEDDLE_CONFIG", user_config.to_str().unwrap())];
    let capture = json_success_with_env(
        &checkout,
        &["--output", "json", "capture", "-m", "isolated audit"],
        &env,
    );
    assert_eq!(capture["principal"]["name"], "Audit User");
    assert_eq!(capture["principal"]["email"], "audit@example.com");

    let parent_head = git_stdout(temp.path(), &["rev-parse", "HEAD"]);
    let output = heddle_output_with_env(
        &["--output", "json", "commit", "-m", "isolated audit"],
        Some(&checkout),
        &env,
    )
    .expect("isolated commit refusal should run");
    assert!(
        !output.status.success(),
        "a native isolated checkout must not write the parent Git history: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let refusal: Value =
        serde_json::from_slice(&output.stderr).expect("isolated commit refusal should be JSON");
    assert_eq!(refusal["kind"], "commit_requires_git_overlay");
    assert_eq!(git_stdout(temp.path(), &["rev-parse", "HEAD"]), parent_head);
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
    let capture = json(temp.path(), &["capture", "-m", "repo identity capture"]);
    json(temp.path(), &["commit", "-m", "repo identity commit"]);

    assert_eq!(capture["principal"]["name"], "Repo Principal");
    assert_eq!(capture["principal"]["email"], "repo@example.com");
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
    let env = [
        ("GIT_CONFIG_GLOBAL", global_config.to_str().unwrap()),
        ("HOME", global_home.path().to_str().unwrap()),
        ("XDG_CONFIG_HOME", global_home.path().to_str().unwrap()),
    ];
    let capture = json_success_with_env(
        temp.path(),
        &[
            "--output",
            "json",
            "capture",
            "-m",
            "global identity capture",
        ],
        &env,
    );
    json_success_with_env(
        temp.path(),
        &["--output", "json", "commit", "-m", "global identity commit"],
        &env,
    );

    assert_eq!(capture["principal"]["name"], "Global User");
    assert_eq!(capture["principal"]["email"], "global@example.com");
    let identity = git_stdout(temp.path(), &["log", "-1", "--format=%an <%ae>%n%cn <%ce>"]);
    assert_eq!(
        identity,
        "Global User <global@example.com>\nGlobal User <global@example.com>"
    );
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
    let env = [
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("HOME", global_home.path().to_str().unwrap()),
        ("XDG_CONFIG_HOME", global_home.path().to_str().unwrap()),
        ("HEDDLE_PRINCIPAL_NAME", "Heddle Principal"),
        ("HEDDLE_PRINCIPAL_EMAIL", "principal@example.com"),
    ];
    let capture = json_success_with_env(
        temp.path(),
        &[
            "--output",
            "json",
            "capture",
            "-m",
            "heddle principal capture",
        ],
        &env,
    );
    json_success_with_env(
        temp.path(),
        &[
            "--output",
            "json",
            "commit",
            "-m",
            "heddle principal commit",
        ],
        &env,
    );
    assert_eq!(capture["principal"]["name"], "Heddle Principal");
    assert_eq!(capture["principal"]["email"], "principal@example.com");
    let identity = git_stdout(temp.path(), &["log", "-1", "--format=%an <%ae>%n%cn <%ce>"]);
    assert_eq!(
        identity,
        "Heddle Principal <principal@example.com>\nHeddle Principal <principal@example.com>"
    );
}

#[test]
fn git_overlay_matrix_capture_without_any_identity_refuses_before_state_change() {
    let temp = TempDir::new().unwrap();
    let global_home = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    git_commit_all(temp.path(), "seed");
    git(&["config", "--unset", "user.name"], temp.path());
    git(&["config", "--unset", "user.email"], temp.path());

    let before_head = git_stdout(temp.path(), &["rev-parse", "HEAD"]);
    initialize_git_overlay(temp.path());
    let before_state = json(temp.path(), &["status", "--output", "json"])["current_state"].clone();

    std::fs::write(temp.path().join("no-identity.txt"), "anonymous?\n").unwrap();
    let output = heddle_output_with_env(
        &[
            "--output",
            "json",
            "capture",
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
    .expect("heddle capture should run");
    assert!(
        !output.status.success(),
        "capture should refuse missing identity"
    );
    assert!(output.stdout.is_empty());
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("missing identity should emit JSON envelope");
    assert_eq!(envelope["kind"], "capture_identity_required");
    assert!(
        envelope["unsafe_condition"]
            .as_str()
            .is_some_and(|condition| condition.contains("Unknown <unknown@example.com>")),
        "capture refusal should name the unsafe fallback: {stderr}"
    );
    assert_eq!(git_stdout(temp.path(), &["rev-parse", "HEAD"]), before_head);
    assert_eq!(
        json(temp.path(), &["status", "--output", "json"])["current_state"],
        before_state,
        "missing identity refusal must happen before changing the Heddle state"
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
    assert_eq!(status["verification"]["status"], "needs_init");
    assert_eq!(status["recommended_action"], "heddle init");
    assert_eq!(status["verification"]["recommended_action"], "heddle init");
    assert!(
        status["verification"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check["name"] == "Mapping" && check["status"] == "git_backed"),
        "unborn Git repos should describe direct Git-backed refs after initialization: {status}"
    );
    assert!(
        !temp.path().join(".heddle").exists(),
        "status in a plain Git repo must be probe-only"
    );
    let status_text = heddle(&["status", "--output", "text"], Some(temp.path())).unwrap();
    assert!(
        status_text.contains("initialize Heddle with heddle init")
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
            && !verify_text.contains("connect this branch with heddle adopt"),
        "unborn verify text should describe initialization, not adoption: {verify_text}"
    );
    let bridge = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(bridge["recommended_action"], "heddle init");
    assert_eq!(bridge["verification"]["recommended_action"], "heddle init");
    assert_eq!(bridge["verification"]["import_state"], "git_backed");
    assert_eq!(bridge["verification"]["mapping_state"], "git_backed");
    assert_no_legacy_verification_sidecars(&bridge);
    let bridge_text = heddle(&["status", "--output", "text"], Some(temp.path())).unwrap();
    assert!(
        bridge_text.contains("heddle init") && !bridge_text.contains("heddle adopt"),
        "unborn status text should not recommend invalid adoption: {bridge_text}"
    );
    let doctor = json(temp.path(), &["doctor", "--output", "json"]);
    assert_eq!(doctor["recommended_action"], "heddle init");
    assert_no_legacy_verification_sidecars(&doctor);

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

    let doctor = json(temp.path(), &["doctor", "--output", "json"]);
    assert_git_overlay_basics(&doctor);
    assert_eq!(doctor["thread"]["name"], "trunk");

    let thread_list = json(temp.path(), &["thread", "list", "--output", "json"]);
    assert_eq!(thread_list["current"], "trunk");

    let workspace = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(workspace["thread"], "trunk");

    let show = json(temp.path(), &["show", "HEAD", "--output", "json"]);
    assert_git_overlay_basics(&show);
    assert!(show["state_id"].as_str().is_some());

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
    assert_eq!(status["recommended_action"], "heddle init");
    assert_eq!(
        status["recommended_action_template"]["argv_template"],
        heddle_argv_json(["init"])
    );
    assert_eq!(status["verification"]["recommended_action"], "heddle init");
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
    assert_eq!(verify["recommended_action"], "heddle init");
    assert_verify_check_rows(&verify);
    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["verification"]["verified"], false);
    assert_eq!(status["verification"]["status"], "needs_init");
    assert_eq!(status["recommended_action"], "heddle init");
    assert_eq!(status["recovery_commands"][0], "heddle init");
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
        status_text.contains("heddle init") && !status_text.contains("heddle adopt --ref main"),
        "plain Git status should name initialization, not adoption: {status_text}"
    );

    let bridge = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(bridge["verification"]["status"], "needs_init");
    assert_eq!(bridge["verification"]["import_state"], "git_backed");
    assert_eq!(bridge["verification"]["mapping_state"], "git_backed");
    assert_no_legacy_verification_sidecars(&bridge);
    assert_verify_check_rows(&bridge["verification"]);
    assert!(
        !temp.path().join(".heddle").exists(),
        "status in a plain Git repo must be observe-only"
    );

    heddle(&["init"], Some(temp.path())).unwrap();
    let verify = json(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(verify["verified"], true);
    assert_eq!(verify["status"], "clean");
    assert_verify_check_rows(&verify);
    assert_eq!(verify["recommended_action"], Value::Null);
    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["verification"]["verified"], true);
    assert_eq!(status["verification"]["status"], "clean");
    assert_eq!(status["recommended_action"], Value::Null);
    assert!(status["recovery_commands"].as_array().unwrap().is_empty());
    assert_verify_check_rows(&status["verification"]);
    let status_text = heddle(&["status", "--output", "text"], Some(temp.path())).unwrap();
    assert_eq!(
        status_text.matches("Setup needed:").count(),
        0,
        "direct-backed status should not duplicate setup advice: {status_text}"
    );
    assert!(
        !status_text.contains("heddle adopt --ref main"),
        "direct-backed status should not require adoption: {status_text}"
    );
    let workspace = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(workspace["verification"]["verified"], true);
    assert_eq!(workspace["verification"]["status"], "clean");
    assert_eq!(workspace["recommended_action"], Value::Null);
    let thread_list = json(temp.path(), &["thread", "list", "--output", "json"]);
    assert!(
        thread_list["threads"]
            .as_array()
            .unwrap()
            .iter()
            .all(|thread| thread["recommended_action"] == Value::Null),
        "thread list should not invent import repair actions for direct-backed refs: {thread_list}"
    );
    assert_verify_check_rows(&workspace["verification"]);
    assert_eq!(thread_list["verification"]["verified"], true);
    assert_eq!(thread_list["verification"]["status"], "clean");
    assert_eq!(thread_list["recommended_action"], Value::Null);
    assert!(
        thread_list["threads"]
            .as_array()
            .unwrap()
            .iter()
            .all(|thread| thread["recommended_action"] == Value::Null),
        "thread list should stay clean for direct-backed refs: {thread_list}"
    );
    assert_verify_check_rows(&thread_list["verification"]);
    let thread_show = json(temp.path(), &["thread", "show", "main", "--output", "json"]);
    assert_eq!(thread_show["verification"]["verified"], true);
    assert_eq!(thread_show["verification"]["status"], "clean");
    assert_eq!(thread_show["recommended_action"], Value::Null);
    assert_verify_check_rows(&thread_show["verification"]);
    let doctor = json(temp.path(), &["doctor", "--output", "json"]);
    assert_eq!(doctor["verification"]["verified"], true);
    assert_eq!(doctor["verification"]["status"], "clean");
    assert_eq!(doctor["verification"]["recommended_action"], Value::Null);
    assert_eq!(doctor["recommended_action"], "heddle ready --thread main");
    assert!(
        doctor["recovery_commands"]
            .as_array()
            .unwrap()
            .iter()
            .all(|command| command
                .as_str()
                .is_some_and(|command| !command.contains("heddle adopt"))),
        "clean direct-backed diagnostics should not require adoption: {doctor}"
    );
    let bridge = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(bridge["verification"]["status"], "clean");
    assert_eq!(bridge["recommended_action"], Value::Null);
    assert!(bridge["recovery_commands"].as_array().unwrap().is_empty());
    let status_text = heddle(&["status", "--output", "text"], Some(temp.path())).unwrap();
    assert!(
        !status_text.contains("heddle bridge git init"),
        "initialized-but-unimported status should not recommend retired bridge git init ceremony: {status_text}"
    );
    assert!(
        !status_text.contains("heddle adopt --ref main"),
        "status text should not require import for direct-backed refs: {status_text}"
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
    let doctor = json(temp.path(), &["doctor", "--output", "json"]);
    assert_eq!(doctor["output_kind"], "doctor");
    assert_eq!(doctor["verification"]["verified"], true);
    assert_eq!(doctor["verification"]["status"], "clean");
    assert_eq!(doctor["recommended_action"], Value::Null);
    assert_eq!(doctor["health"]["recommended_action"], Value::Null);
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
    let workspace = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(workspace["output_kind"], "status");
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
    let bridge = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(bridge["output_kind"], "status");
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
    assert_eq!(before["recommended_action"], "heddle init");
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
    let bridge = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(bridge["verification"]["verified"], true);
    assert_no_legacy_verification_sidecars(&bridge);
}

#[test]
fn git_overlay_matrix_verify_reads_git_tags_created_after_adoption() {
    let fixture = GitOverlayFixture::imported_main();

    fixture.git(&["tag", "v2.0.0"]);

    let verify = json(fixture.path(), &["verify", "--output", "json"]);
    assert_eq!(verify["verified"], true);
    assert_eq!(verify["status"], "clean");
    assert_eq!(verify["mapping_state"], "clean");
    assert_eq!(verify["import_state"], "clean");
    assert_eq!(verify["recommended_action"], Value::Null);

    let status = json(fixture.path(), &["status", "--output", "json"]);
    assert_eq!(status["verification"]["status"], "clean");
    assert_eq!(status["recommended_action"], Value::Null);

    let adopted = json(
        fixture.path(),
        &["adopt", "--ref", "v2.0.0", "--output", "json"],
    );
    assert_eq!(adopted["tags_synced"], 1);
    assert_eq!(adopted["verification"]["verified"], true);
    assert_eq!(adopted["verification"]["status"], "clean");
}

#[test]
fn git_overlay_matrix_native_adopted_tag_is_stable_when_git_projection_moves() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "one\n").unwrap();
    git_commit_all(temp.path(), "one");
    git(&["tag", "v1.0.0"], temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "two\n").unwrap();
    git_commit_all(temp.path(), "two");

    let adopted = json(temp.path(), &["adopt", "--output", "json"]);
    assert_eq!(adopted["adopted"], true);
    let tag_before = json(temp.path(), &["show", "v1.0.0", "--output", "json"]);
    let state_before = tag_before["state_id"]
        .as_str()
        .expect("adopted tag should resolve to native state")
        .to_string();

    git(&["tag", "-f", "v1.0.0", "HEAD"], temp.path());

    let verify = json(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(verify["verified"], true);
    assert_eq!(verify["status"], "clean");
    let tag_after = json(temp.path(), &["show", "v1.0.0", "--output", "json"]);
    assert_eq!(
        tag_after["state_id"], state_before,
        "native Heddle tags must not move when the Git projection changes"
    );
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
    assert_eq!(adopted["verification"]["verified"], true);
    assert_eq!(adopted["verification"]["status"], "clean");
    assert_eq!(adopted["recommended_action"], Value::Null);
}

#[test]
fn git_overlay_matrix_new_branch_at_adopted_tip_verifies_without_setup_loop() {
    let fixture = GitOverlayFixture::imported_main();

    let adopted = json(fixture.path(), &["show", "HEAD", "--output", "json"]);
    let adopted_change = adopted["state_id"]
        .as_str()
        .expect("adopted state should have short change id")
        .to_string();

    fixture.git(&["checkout", "-b", "scratch"]);

    let status = json(fixture.path(), &["status", "--output", "json"]);
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
    assert!(status["state"]["state_id"].as_str().is_some());
    assert!(status["current_state"].as_str().is_some());

    let status_text = fixture.heddle(&["status", "--output", "text"]).unwrap();
    assert!(
        status_text.contains("Heddle status for scratch")
            && status_text.contains("Checkout: Git branch checkout")
            && !status_text.contains("Setup needed")
            && !status_text.contains("main checkout")
            && !status_text.contains("heddle adopt --ref scratch"),
        "status text should agree with the checked-out Git branch without repeating setup copy: {status_text}"
    );

    let bridge = json(fixture.path(), &["status", "--output", "json"]);
    assert_eq!(bridge["verification"]["verified"], true);
    assert_eq!(bridge["verification"]["status"], "clean");
    assert_no_legacy_verification_sidecars(&bridge);

    std::fs::write(fixture.path().join("scratch.txt"), "scratch\n").unwrap();
    fixture.heddle(&["capture", "-m", "scratch work"]).unwrap();
    let captured = json(fixture.path(), &["show", "HEAD", "--output", "json"]);
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
    let fixture = GitOverlayFixture::imported_main();
    std::fs::write(fixture.path().join("README.md"), "changed\n").unwrap();
    json(
        fixture.path(),
        &["--output", "json", "capture", "-m", "change"],
    );

    let commit = json(
        fixture.path(),
        &["--output", "json", "commit", "-m", "change"],
    );
    assert_eq!(commit["output_kind"], "commit");
    assert!(commit["state_id"].as_str().is_some());
    assert!(commit["git_commit"].as_str().is_some());
    assert_eq!(commit["verification"]["verified"], true);
    assert_eq!(commit["verification"]["status"], "clean");
    assert_eq!(
        commit["verification"]["recommended_action"],
        Value::Null,
        "commit after single-ref adoption should checkpoint instead of falling into needs_import: {commit}"
    );
    assert_eq!(git_status_short(fixture.path()), "");

    let verify = json(fixture.path(), &["verify", "--output", "json"]);
    assert_eq!(verify["verified"], true);
    assert_eq!(verify["status"], "clean");
    assert_eq!(verify["recommended_action"], Value::Null);
}

#[test]
fn git_overlay_matrix_ready_is_clean_after_direct_backed_init() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle(&["init"], Some(temp.path())).unwrap();

    let ready = json(temp.path(), &["--output", "json", "ready"]);
    assert_eq!(ready["status"], "completed", "{ready}");
    assert_eq!(ready["verification"]["verified"], true);
    assert_eq!(ready["verification"]["status"], "clean");
    assert_eq!(ready["recommended_action"], Value::Null);
    assert!(
        ready["message"]
            .as_str()
            .is_some_and(|message| message.contains("clean")),
        "ready should report a clean no-target state: {ready}"
    );
    assert_verify_check_rows(&ready["verification"]);
}

#[test]
fn git_overlay_matrix_ready_thread_keeps_verification_clean_and_workflow_actionable() {
    let fixture =
        GitOverlayFixture::imported_main().with_ready_materialized_thread("feature/ready-verify");
    let thread_path = fixture.ready_thread_path();

    let ready = json(thread_path, &["--output", "json", "ready"]);
    assert_eq!(ready["status"], "completed");
    assert_eq!(ready["verification"]["verified"], true);
    assert_eq!(ready["verification"]["workflow_status"], "ready");
    assert!(
        ready["recommended_action"]
            .as_str()
            .is_some_and(
                |action| action.contains("land --thread feature/ready-verify")
                    && !action.contains("--no-push")
            ),
        "ready should expose the direct land action: {ready}"
    );

    let parent_status = json(fixture.path(), &["--output", "json", "status"]);
    assert_eq!(
        parent_status["recommended_action"],
        "heddle land --thread feature/ready-verify"
    );
    let thread_list = json(fixture.path(), &["--output", "json", "thread", "list"]);
    assert_eq!(
        thread_list["recommended_action"],
        "heddle land --thread feature/ready-verify"
    );

    let landed = json(
        fixture.path(),
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/ready-verify",
        ],
    );
    assert_eq!(landed["output_kind"], "land");
    assert_eq!(landed["verification"]["verified"], true);
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
    initialize_git_overlay(temp.path());
    heddle(&["import", "git", "--ref", "main"], Some(temp.path())).unwrap();

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
        &["--output", "json", "capture", "-m", "feature work"],
    );

    std::fs::write(temp.path().join("main.txt"), "main change\n").unwrap();
    json(
        temp.path(),
        &["--output", "json", "capture", "-m", "main work"],
    );
    json(temp.path(), &["--output", "json", "commit"]);
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
    assert_eq!(ready["status"], "completed", "{ready}");
    assert_eq!(
        ready["recommended_action"], "heddle land --thread feature/stale-ready",
        "thread-scoped ready should refresh clean stale work and keep land primary, not global push: {ready}"
    );
    assert_eq!(
        ready["report"]["recommended_action"], "heddle land --thread feature/stale-ready",
        "nested report should match the top-level ready action: {ready}"
    );
    assert_eq!(ready["report"]["freshness"], "current", "{ready}");
}

#[test]
fn git_overlay_matrix_thread_and_workspace_plain_git_are_observe_only() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");

    let thread_list = json(temp.path(), &["thread", "list", "--output", "json"]);
    assert_eq!(thread_list["repository_capability"], "plain-git");
    assert_eq!(thread_list["recommended_action"], "heddle init");
    assert_eq!(
        thread_list["recommended_action_template"]["argv_template"],
        heddle_argv_json(["init"])
    );
    assert!(
        !temp.path().join(".heddle").exists(),
        "thread list in a plain Git repo must be observe-only"
    );

    let thread_show = json(temp.path(), &["thread", "show", "main", "--output", "json"]);
    assert_eq!(thread_show["repository_capability"], "plain-git");
    assert_eq!(thread_show["recommended_action"], "heddle init");
    assert_eq!(
        thread_show["recommended_action_template"]["argv_template"],
        heddle_argv_json(["init"])
    );
    assert!(
        !temp.path().join(".heddle").exists(),
        "thread show in a plain Git repo must be observe-only"
    );

    let workspace = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(workspace["repository_capability"], "plain-git");
    assert_eq!(workspace["verification"]["status"], "needs_init");
    assert_eq!(workspace["recommended_action"], "heddle init");
    assert_eq!(
        workspace["recommended_action_template"]["argv_template"],
        heddle_argv_json(["init"])
    );
    assert_verify_check_rows(&workspace["verification"]);
    assert!(
        !temp.path().join(".heddle").exists(),
        "status in a plain Git repo must be observe-only"
    );
}

#[test]
fn git_overlay_matrix_observe_only_contract_preserves_plain_git_repo() {
    let catalog: Value =
        serde_json::from_str(&heddle(&["help", "--output", "json"], None).unwrap())
            .expect("command catalog should be JSON");
    let commands = catalog["commands"]
        .as_array()
        .expect("catalog commands should be an array");
    let cases: &[(&str, &[&str])] = &[
        ("status", &["status", "--output", "json"]),
        ("doctor", &["doctor", "--output", "json"]),
        ("doctor", &["doctor", "--output", "json"]),
        ("status", &["status", "--output", "json"]),
        ("verify", &["verify", "--output", "json"]),
        ("thread list", &["thread", "list", "--output", "json"]),
        (
            "thread show",
            &["thread", "show", "main", "--output", "json"],
        ),
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
fn git_overlay_matrix_native_git_import_materializes_current_thread_when_clean() {
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
            "--output", "json", "import", "git", "--path", source_arg, "--ref", "main",
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
    assert_eq!(status["verification"]["status"], "clean");
    assert_eq!(status["recommended_action"], Value::Null);

    let reconcile = json(
        temp.path(),
        &["fsck", "repair", "git", "--prefer", "git", "--ref", "main"],
    );
    assert_eq!(reconcile["repair_target"], "git");
    assert_eq!(reconcile["valid"], true);
    assert_eq!(
        reconcile["repairs"][0]["name"],
        "git_projection_ref_prefer_git"
    );

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["verification"]["status"], "clean");
    assert_eq!(status["thread"], "main");
}

#[test]
fn git_overlay_matrix_reconcile_prefer_heddle_requires_adoption() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &[
            "--output", "json", "fsck", "repair", "git", "--prefer", "heddle", "--ref", "main",
        ],
        Some(temp.path()),
    )
    .expect("invoke reconcile");
    assert!(
        !output.status.success(),
        "preferring Heddle under Git authority should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode reconcile refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("authority refusal should emit JSON envelope");
    assert_eq!(envelope["kind"], "git_repair_requires_adoption");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("Git owns source history")),
        "reconcile refusal should name source authority: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle adopt")),
        "reconcile hint should require adoption: {stderr}"
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
    heddle(&["import", "git", "--ref", "main"], Some(temp.path())).unwrap();

    std::fs::create_dir(temp.path().join("__pycache__")).unwrap();
    std::fs::write(temp.path().join("__pycache__/tracked.pyc"), "cache").unwrap();
    let before_head = git_stdout(temp.path(), &["rev-parse", "HEAD"]);

    let output = heddle_output(
        &["--output", "json", "commit", "-m", "noop"],
        Some(temp.path()),
    )
    .expect("commit should run");
    assert!(
        output.status.success(),
        "ignored-only commit should be a no-op"
    );
    assert!(
        output.stderr.is_empty(),
        "JSON-mode no-op commit should keep stderr quiet: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(git_stdout(temp.path(), &["rev-parse", "HEAD"]), before_head);
}

#[test]
fn git_overlay_matrix_commit_requires_explicit_ignore_for_python_generated_noise() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(&["import", "git", "--ref", "main"], Some(temp.path())).unwrap();

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
    json(
        temp.path(),
        &["--output", "json", "capture", "-m", "capture generated"],
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
    heddle(&["import", "git", "--ref", "main"], Some(temp.path())).unwrap();

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
            .is_some_and(|error| error.contains("externally-started Git merge")),
        "verify-blocked no-op commit should refuse with full typed advice: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle verify")
                && hint.contains("finish or abort it with the Git-compatible tool")),
        "verify-blocked no-op commit should name the verify recovery command: {stderr}"
    );
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

    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(&["import", "git", "--ref", "main"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "two\n").unwrap();
    json(
        temp.path(),
        &["--output", "json", "capture", "-m", "change"],
    );
    let commit = json(temp.path(), &["--output", "json", "commit", "-m", "change"]);
    assert_eq!(commit["output_kind"], "commit");
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
    assert_eq!(before_push["verified"], true, "{before_push}");
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
    initialize_git_overlay(temp.path());
    heddle(&["import", "git", "--ref", "main"], Some(temp.path())).unwrap();

    let peer_arg = peer.path().to_str().expect("peer path should be utf8");
    git(&["clone", origin_arg, peer_arg], temp.path());
    git(&["config", "user.name", "Peer"], peer.path());
    git(&["config", "user.email", "peer@example.com"], peer.path());

    std::fs::write(temp.path().join("tracked.txt"), "local\n").unwrap();
    json(
        temp.path(),
        &["--output", "json", "capture", "-m", "local checkpoint"],
    );
    json(
        temp.path(),
        &["--output", "json", "commit", "-m", "local checkpoint"],
    );
    let git_head_before = git_stdout(temp.path(), &["rev-parse", "HEAD"]);

    std::fs::write(peer.path().join("tracked.txt"), "remote\n").unwrap();
    git_commit_all(peer.path(), "remote checkpoint");
    git(&["push", "origin", "main"], peer.path());
    git(&["fetch", "origin"], temp.path());
    let verify = json(temp.path(), &["--output", "json", "verify"]);
    assert_eq!(verify["remote_drift"], "remote_diverged", "{verify}");

    std::fs::write(temp.path().join("extra.txt"), "blocked\n").unwrap();
    json(
        temp.path(),
        &["--output", "json", "capture", "-m", "should not commit"],
    );
    let state_before = state_chain_ids(temp.path(), 8);
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
        envelope["primary_command"], "heddle import git --ref origin/main",
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
    assert_eq!(
        status["verification"]["status"], "needs_checkpoint",
        "the explicit capture must remain available after commit refuses remote divergence: {status}"
    );
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

    initialize_git_overlay(temp.path());
    heddle(&["import", "git", "--ref", "main"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "two\n").unwrap();
    json(
        temp.path(),
        &["--output", "json", "capture", "-m", "change"],
    );
    json(temp.path(), &["--output", "json", "commit", "-m", "change"]);
    let before_push = json(temp.path(), &["--output", "json", "verify"]);
    assert_eq!(before_push["verified"], true, "{before_push}");
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

    initialize_git_overlay(temp.path());
    heddle(&["import", "git", "--ref", "main"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "two\n").unwrap();
    json(
        temp.path(),
        &["--output", "json", "capture", "-m", "change"],
    );
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

    initialize_git_overlay(temp.path());
    heddle(&["import", "git", "--ref", "main"], Some(temp.path())).unwrap();
    let audit_arg = audit.path().to_str().expect("audit path should be utf8");
    let added = json(
        temp.path(),
        &["--output", "json", "remote", "add", "audit", audit_arg],
    );
    assert_eq!(added["output_kind"], "remote_add");
    assert_eq!(added["default"], "audit");
    assert_eq!(added["verification"]["default_remote"], "audit", "{added}");
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
    initialize_git_overlay(temp.path());

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
    initialize_git_overlay(temp.path());

    let staging_arg = staging
        .path()
        .to_str()
        .expect("staging path should be utf8");
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
    initialize_git_overlay(temp.path());

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
fn git_overlay_matrix_remote_set_default_unknown_returns_not_found() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "one\n").unwrap();
    git_commit_all(temp.path(), "seed");
    initialize_git_overlay(temp.path());

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
fn git_overlay_matrix_subdirectory_dirty_commands() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    initialize_git_overlay(temp.path());

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

    let doctor = json(&nested, &["doctor", "--output", "json"]);
    assert_eq!(doctor["changes"]["total"], 2);

    let show = json(&nested, &["show", "HEAD", "--output", "json"]);
    assert!(show["state_id"].as_str().is_some());

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

    let workspace = json(&nested, &["status", "--output", "json"]);
    assert_eq!(workspace["thread"], "feature/drop-in");
}

#[test]
fn git_overlay_matrix_manual_git_commit_after_bootstrap_commands() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    initialize_git_overlay(temp.path());
    heddle(
        &["import", "git", "--ref", "feature/drop-in"],
        Some(temp.path()),
    )
    .unwrap();

    std::fs::write(temp.path().join("tracked.txt"), "tracked committed via git").unwrap();
    git(&["add", "tracked.txt"], temp.path());
    git(&["commit", "-m", "manual git commit"], temp.path());

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["thread"], "feature/drop-in");
    assert_eq!(status["verification"]["status"], "clean");
    assert_eq!(status["verification"]["mapping_state"], "git_backed");
    assert_eq!(status["verification"]["import_state"], "clean");
    assert_eq!(
        status["changed_path_count"], 0,
        "a clean Git worktree with an unimported Git commit should not look like unsaved Heddle work: {status}"
    );
    assert!(
        status["changes"]["modified"].as_array().unwrap().is_empty(),
        "branch-tip drift should not be reported as unsaved modified paths: {status}"
    );
    assert_eq!(status["recommended_action"], Value::Null);
    assert_eq!(status["recommended_action_template"], Value::Null);
    assert_eq!(status["verification"]["recommended_action"], Value::Null);
    assert_eq!(status["verification"]["workflow_status"], "clean");
    assert_eq!(status["verification"]["worktree_state"], "clean");
    let status_text = heddle(&["status", "--output", "text", "-v"], Some(temp.path())).unwrap();
    assert!(
        status_text.contains("Verdict: clean")
            && status_text.contains("Health: clean")
            && !status_text.contains("heddle adopt --ref feature/drop-in")
            && !status_text.contains("Setup needed: Git repo detected")
            && !status_text.contains("Changes not yet saved"),
        "text status should treat direct Git-backed commits as clean: {status_text}"
    );

    let verify = json(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(verify["status"], "clean");
    assert_eq!(verify["mapping_state"], "git_backed");
    assert_eq!(verify["recommended_action"], Value::Null);
    let verify_text_output = heddle_output(&["verify", "--output", "text"], Some(temp.path()))
        .expect("invoke strict verify text");
    assert!(
        verify_text_output.status.success(),
        "direct Git-backed verify text should succeed"
    );
    let verify_text = String::from_utf8_lossy(&verify_text_output.stdout);
    assert!(
        verify_text.contains("Workspace: verified")
            && !verify_text.contains("heddle adopt --ref feature/drop-in")
            && !verify_text.contains("Setup needed: Git repo detected"),
        "verify text should treat direct Git-backed commits as clean: {verify_text}"
    );

    let bridge = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(bridge["verification"]["status"], "clean");
    assert_eq!(bridge["recommended_action"], Value::Null);
    let bridge_text = heddle(&["status", "--output", "text"], Some(temp.path())).unwrap();
    assert!(
        (bridge_text.contains("Verdict: clean") || bridge_text.contains("Health: clean"))
            && !bridge_text.contains("Recovery: heddle adopt --ref feature/drop-in")
            && !bridge_text.contains("Setup needed"),
        "status text should treat direct Git-backed commits as clean: {bridge_text}"
    );

    let show = json(temp.path(), &["show", "HEAD", "--output", "json"]);
    assert!(show["state_id"].as_str().is_some());

    let log = json(temp.path(), &["log", "--output", "json"]);
    assert!(
        !log["states"].as_array().unwrap().is_empty(),
        "log should still succeed after plain git commits: {log}"
    );

    let same_state_diff = json(temp.path(), &["diff", "HEAD", "HEAD"]);
    assert_eq!(same_state_diff["stats"]["files_changed"], 0);

    let doctor = json(temp.path(), &["doctor", "--output", "json"]);
    assert_eq!(
        doctor["changes"]["total"], 0,
        "doctor must not resurrect stale Heddle-vs-state paths when Git is clean: {doctor}"
    );
    let diff = json(temp.path(), &["diff", "--output", "json", "--stat"]);
    let diff_changes = diff["changes"]
        .as_object()
        .unwrap_or_else(|| panic!("worktree diff changes should be a category object: {diff}"));
    assert!(
        ["modified", "added", "deleted"]
            .iter()
            .all(|key| diff_changes[*key].as_array().is_some_and(|a| a.is_empty())),
        "diff must not report stale paths when Git is clean: {diff}"
    );

    std::fs::write(temp.path().join("tracked.txt"), "dirty after manual git\n").unwrap();
    let dirty_status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(dirty_status["changed_path_count"], 1);
    assert_eq!(dirty_status["verification"]["worktree_state"], "dirty");
    let dirty_doctor = json(temp.path(), &["doctor", "--output", "json"]);
    assert_eq!(
        dirty_doctor["changes"]["total"], 1,
        "doctor should show the same current Git dirty set as status: {dirty_doctor}"
    );
    let dirty_diff = json(temp.path(), &["diff", "--output", "json", "--stat"]);
    let dirty_changes = dirty_diff["changes"].as_object().unwrap_or_else(|| {
        panic!("worktree diff changes should be a category object: {dirty_diff}")
    });
    let dirty_total: usize = ["modified", "added", "deleted"]
        .iter()
        .filter_map(|key| dirty_changes[*key].as_array())
        .map(Vec::len)
        .sum();
    assert_eq!(
        dirty_total, 1,
        "diff should show the same current Git dirty set as status: {dirty_diff}"
    );

    let ready = json(
        temp.path(),
        &["--output", "json", "ready", "-m", "carry branch work"],
    );
    assert_eq!(ready["status"], "blocked", "{ready}");
    assert_eq!(ready["captured"], true, "{ready}");
    assert!(
        ready["recommended_action"]
            .as_str()
            .is_some_and(|action| action.contains("heddle commit")),
        "captured overlay work should recommend committing it to Git: {ready}"
    );
}

/// Full out-of-band round trip (#534): adopt → plain-git commits → detection
/// reports the out-of-band commit count → the recommended one-line
/// `heddle adopt --ref` reconcile → state verified back in sync with the Git
/// branch SHA untouched.
#[test]
fn git_overlay_matrix_manual_git_commits_reconcile_round_trip() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed branch");
    initialize_git_overlay(temp.path());

    std::fs::write(temp.path().join("tracked.txt"), "first manual git edit\n").unwrap();
    git_commit_all(temp.path(), "manual git commit 1");
    std::fs::write(temp.path().join("second.txt"), "second manual file\n").unwrap();
    git_commit_all(temp.path(), "manual git commit 2");
    let out_of_band_head = git_stdout(temp.path(), &["rev-parse", "HEAD"]);

    // Detection leg: divergence is reported with how far git moved.
    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["verification"]["status"], "clean");
    assert_eq!(status["recommended_action"], Value::Null);

    // Explicit conversion leg: `adopt --ref` still imports the Git tip into
    // Heddle-native state when requested.
    let adopted = json(
        temp.path(),
        &["adopt", "--ref", "feature/drop-in", "--output", "json"],
    );
    assert_eq!(
        adopted["verification"]["verified"], true,
        "reconcile should return clean post-adoption verification: {adopted}"
    );
    assert_eq!(adopted["verification"]["status"], "clean");

    // Round trip closed: the Git branch SHA is untouched and Heddle agrees.
    assert_eq!(
        git_stdout(temp.path(), &["rev-parse", "HEAD"]),
        out_of_band_head,
        "reconcile must import the out-of-band tip, not rewrite Git history"
    );
    assert!(
        !temp.path().join(".heddle/git").exists(),
        "ingest-backed import should not recreate the legacy legacy Bridge Mirror"
    );
    let verify = json(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(verify["verified"], true);
    assert_eq!(verify["status"], "clean");
    let status_after = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status_after["verification"]["status"], "clean");
    assert_eq!(status_after["verification"]["verified"], true);
    assert_eq!(
        status_after["changed_path_count"], 0,
        "a reconciled checkout should have nothing left to save: {status_after}"
    );
}

#[test]
fn git_overlay_matrix_raw_git_reset_reports_reconcile_not_unsaved_work() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
    git_commit_all(temp.path(), "seed");
    initialize_git_overlay(temp.path());
    heddle(&["import", "git", "--ref", "main"], Some(temp.path())).unwrap();

    std::fs::write(temp.path().join("tracked.txt"), "heddle change\n").unwrap();
    json(
        temp.path(),
        &["--output", "json", "capture", "-m", "heddle change"],
    );
    let committed = json(
        temp.path(),
        &["--output", "json", "commit", "-m", "heddle change"],
    );
    let heddle_state = committed["state_id"]
        .as_str()
        .expect("commit should report Heddle state")
        .to_string();

    git(&["reset", "--hard", "HEAD~1"], temp.path());
    assert_eq!(git_status_short(temp.path()), "");
    let reset_head = git_stdout(temp.path(), &["rev-parse", "HEAD"]);

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["verification"]["status"], "needs_reconcile");
    assert_eq!(status["verification"]["mapping_state"], "needs_reconcile");
    assert_eq!(status["changed_path_count"], 0);
    assert!(status["changes"]["modified"].as_array().unwrap().is_empty());
    assert!(status["changes"]["added"].as_array().unwrap().is_empty());
    assert!(status["changes"]["deleted"].as_array().unwrap().is_empty());
    assert_eq!(
        status["recommended_action"],
        "heddle fsck repair git --ref main --preview"
    );
    assert_eq!(
        status["recommended_action_template"]["argv_template"],
        heddle_argv_json(["fsck", "repair", "git", "--ref", "main", "--preview"])
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
        "heddle fsck repair git --ref main --preview"
    );

    let bridge = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(bridge["verification"]["status"], "needs_reconcile");
    assert_eq!(
        bridge["recommended_action"],
        "heddle fsck repair git --ref main --preview"
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
    assert_eq!(envelope["kind"], "git_checkpoint_preflight_blocked");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("Refusing to commit")),
        "commit should refuse as commit, not leak capture wording: {envelope}"
    );
    assert_eq!(
        envelope["primary_command"],
        "heddle fsck repair git --ref main --preview"
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

    // Import-hint information has moved to `heddle status
    // --output json`; per-command outputs (status, log, show, workspace,
    // thread list) no longer carry it.
    git(&["branch", "support/original"], temp.path());
    let bridge_before = json(temp.path(), &["status", "--output", "json"]);
    assert_no_legacy_verification_sidecars(&bridge_before);
    assert_eq!(bridge_before["verification"]["status"], "needs_init");

    git(
        &["branch", "-m", "support/original", "support/renamed"],
        temp.path(),
    );
    let bridge_after_rename = json(temp.path(), &["status", "--output", "json"]);
    assert_no_legacy_verification_sidecars(&bridge_after_rename);

    git(&["branch", "-D", "support/renamed"], temp.path());
    let bridge_after_delete = json(temp.path(), &["status", "--output", "json"]);
    assert_no_legacy_verification_sidecars(&bridge_after_delete);

    git(&["branch", "support/recreated"], temp.path());
    let bridge_after_recreate = json(temp.path(), &["status", "--output", "json"]);
    assert_no_legacy_verification_sidecars(&bridge_after_recreate);
}

#[test]
fn git_overlay_matrix_branch_delete_does_not_recommend_deleted_thread() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    git_commit_all(temp.path(), "seed branch");
    initialize_git_overlay(temp.path());

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
    let text = heddle(
        &["thread", "drop", "feature/delete-text"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        text.contains("Dropped thread 'feature/delete-text'"),
        "thread drop should report the dropped thread: {text}"
    );
    assert!(
        !text.contains("heddle ready --thread feature/delete-text") && !text.contains("Next:"),
        "thread drop must not point at the deleted thread: {text}"
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
        &[
            "--output",
            "json",
            "thread",
            "drop",
            "feature/delete-json",
            "--delete-thread",
        ],
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
        "heddle thread switch support/alpha"
    );

    let beta_show = json(
        temp.path(),
        &["thread", "show", "support/beta", "--output", "json"],
    );
    assert_eq!(beta_show["name"], "support/beta");
    assert_eq!(beta_show["history_imported"], false);
    assert!(beta_show["git_branch_tip"].as_str().is_some());

    let workspace = json(temp.path(), &["thread", "list", "--output", "json"]);
    let workspace_threads = workspace["threads"].as_array().unwrap().to_vec();
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

    // Import-hint information has moved to `heddle status
    // --output json`; per-command outputs no longer carry it.
    let bridge = json(temp.path(), &["status", "--output", "json"]);
    assert_no_legacy_verification_sidecars(&bridge);
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

    heddle(&["import", "git", "--path", "."], Some(temp.path())).unwrap();

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

    let doctor = json(temp.path(), &["doctor", "--output", "json"]);
    assert_eq!(doctor["thread"]["name"], "develop");

    let thread_list = json(temp.path(), &["thread", "list", "--output", "json"]);
    assert_eq!(thread_list["current"], "develop");

    let workspace = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(workspace["thread"], "develop");
}

#[test]
fn git_overlay_matrix_detached_head_sequence_commands() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(
        &["import", "git", "--ref", "feature/drop-in"],
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
        verify["recommended_action"], "heddle thread switch feature/drop-in",
        "detached-head recovery should stay inside Heddle's no-git runtime: {verify}"
    );
    assert_eq!(
        verify["recommended_action_template"]["argv_template"],
        heddle_argv_json(["thread", "switch", "feature/drop-in"])
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
    assert!(show["state_id"].as_str().is_some());

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
    heddle(&["import", "git", "--ref", "main"], Some(temp.path())).unwrap();

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
    assert_eq!(envelope["primary_command"], "heddle thread switch main");
    assert_eq!(
        envelope["primary_command_template"]["argv_template"],
        heddle_argv_json(["thread", "switch", "main"])
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
    assert_eq!(status["verification"]["status"], "detached_head");
    assert!(
        status["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "detached-tag.txt"),
        "status should remain usable when detached at a tag: {status}"
    );

    let doctor = json(temp.path(), &["doctor", "--output", "json"]);
    assert_git_overlay_basics(&doctor);

    let show = json(temp.path(), &["show", "HEAD", "--output", "json"]);
    assert_git_overlay_basics(&show);
    assert!(show["state_id"].as_str().is_some());
}

#[test]
fn git_overlay_matrix_dirty_branch_switch_when_git_allows_carryover() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("shared.txt"), "base").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/carry"], temp.path());
    initialize_git_overlay(temp.path());

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
    json(temp.path(), &["--output", "json", "commit"]);

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

    heddle(&["init"], Some(temp.path())).unwrap();
    let ready = json(
        temp.path(),
        &["--output", "json", "ready", "-m", "First-run capture"],
    );
    assert_eq!(ready["captured"], false, "{ready}");
    assert_eq!(ready["status"], "blocked", "{ready}");
    assert_eq!(ready["recommended_action"], "heddle commit -m \"...\"");

    let commit = json(temp.path(), &["--output", "json", "commit"]);
    assert!(commit["git_commit"].as_str().is_some());

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["verification"]["verified"], true);
    assert_eq!(status["changed_path_count"], 0);
}

#[test]
fn git_overlay_matrix_imported_branch_evolution_after_git_import() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    git(&["branch", "support/alpha"], temp.path());
    git(&["branch", "support/beta"], temp.path());

    let before = json(temp.path(), &["status", "--output", "json"]);
    assert_no_legacy_verification_sidecars(&before);
    assert_eq!(before["verification"]["status"], "needs_init");

    let import_output = heddle(&["import", "git", "--path", "."], Some(temp.path())).unwrap();
    assert!(
        import_output.contains("branches") || import_output.contains("\"branches_synced\""),
        "Git import should report branch sync activity: {import_output}"
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

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_no_legacy_verification_sidecars(&status);
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
fn git_overlay_matrix_reopen_from_different_cwds_preserves_state_and_git_only_aliases() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    initialize_git_overlay(temp.path());
    heddle(
        &["import", "git", "--ref", "feature/drop-in"],
        Some(temp.path()),
    )
    .unwrap();
    git(&["branch", "support/reopen-me"], temp.path());

    let root_status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(root_status["thread"], "feature/drop-in");
    let root_bridge = json(temp.path(), &["status", "--output", "json"]);
    assert_no_legacy_verification_sidecars(&root_bridge);
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
    let nested_workspace = json(&nested, &["status", "--output", "json"]);
    assert_eq!(nested_workspace["thread"], "feature/drop-in");
    let nested_bridge = json(&nested, &["status", "--output", "json"]);
    assert_no_legacy_verification_sidecars(&nested_bridge);

    std::fs::write(temp.path().join("tracked.txt"), "tracked after reopen").unwrap();
    let ready = json(
        &nested,
        &["--output", "json", "ready", "-m", "nested ready capture"],
    );
    assert_eq!(ready["captured"], true);

    let root_show = json(temp.path(), &["show", "HEAD", "--output", "json"]);
    assert!(root_show["state_id"].as_str().is_some());

    let nested_log = json(&nested, &["log", "--output", "json"]);
    assert!(
        !nested_log["states"].as_array().unwrap().is_empty(),
        "reopened nested cwd should still see persisted history: {nested_log}"
    );

    let root_status_after = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(
        root_status_after["worktree_changed_path_count"], 0,
        "{root_status_after}"
    );
    assert_eq!(root_status_after["thread_changed_path_count"], 0);
    let root_bridge_after = json(temp.path(), &["status", "--output", "json"]);
    assert_no_legacy_verification_sidecars(&root_bridge_after);
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
    initialize_git_overlay(temp.path());
    heddle(
        &["import", "git", "--ref", "feature/drop-in"],
        Some(temp.path()),
    )
    .unwrap();

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
    assert_eq!(ready["captured"], true, "{ready}");

    let status_after = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status_after["worktree_changed_path_count"], 0);
    assert_eq!(
        status_after["thread_changed_path_count"], 0,
        "{status_after}"
    );
    assert_eq!(status_after["verification"]["status"], "needs_checkpoint");
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

    initialize_git_overlay(temp.path());

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
    initialize_git_overlay(temp.path());

    // Advance the Git branch outside Heddle (an unrelated commit), then leave
    // a cross-type move dirty in the worktree. Direct-backed Git overlays read
    // the live Git branch without requiring import, and dirty status still
    // routes `heddle diff` through `render_worktree_status_diff` (the path cid
    // 3321103601 flagged), instead of the heddle-native builder.
    std::fs::write(temp.path().join("filler.txt"), "filler edit\n").unwrap();
    git(&["add", "filler.txt"], temp.path());
    git(
        &["commit", "-m", "advance branch outside heddle"],
        temp.path(),
    );

    // The cross-type move stays UNCOMMITTED in the worktree: `linked` follows
    // to `anchor.txt`, so the worktree blob read for the added side equals the
    // removed `mover.txt` bytes — a similarity-1.0 rename candidate that must
    // still stay split across the regular↔symlink boundary.
    std::fs::remove_file(temp.path().join("mover.txt")).unwrap();
    symlink("anchor.txt", temp.path().join("linked")).unwrap();

    // Sanity: confirm we are actually on the `render_worktree_status_diff`
    // path — the direct-backed branch should be readable, with only the live
    // worktree reporting dirty.
    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(
        status["verification"]["status"], "dirty_worktree",
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
    initialize_git_overlay(temp.path());

    symlink("README.md", temp.path().join("link-to-readme")).unwrap();
    let diff = json(temp.path(), &["--output", "json", "diff"]);
    let link_change = diff["changes"]["added"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "link-to-readme")
        .unwrap_or_else(|| {
            panic!("diff should include added symlink under the added category: {diff}")
        });
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
    initialize_git_overlay(temp.path());

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
    assert!(show["state_id"].as_str().is_some());
}

#[cfg(unix)]
#[test]
fn git_overlay_matrix_filemode_changes_surface_and_capture() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("script.sh"), "#!/bin/sh\necho hi\n").unwrap();
    git_commit_all(temp.path(), "seed script");
    initialize_git_overlay(temp.path());

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

    let commit = json(temp.path(), &["--output", "json", "commit"]);
    assert!(commit["git_commit"].as_str().is_some());
}

#[test]
fn git_overlay_matrix_land_checkpoint_failure_auto_undoes_heddle_integration() {
    let fixture =
        GitOverlayFixture::imported_main().with_ready_materialized_thread("feature/land-rollback");
    let before_state =
        json(fixture.path(), &["--output", "json", "status"])["current_state"].clone();
    let before_git = git_stdout(fixture.path(), &["rev-parse", "HEAD"]);
    let before_refs = git_ref_snapshot(fixture.path());

    let land = heddle_output_with_env(
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/land-rollback",
        ],
        Some(fixture.path()),
        &[("HEDDLE_FAULT_INJECT", "git_checkpoint_before_write_through")],
    )
    .expect("invoke land with checkpoint fault injection");
    assert!(!land.status.success());
    assert!(land.stdout.is_empty());
    let stderr = std::str::from_utf8(&land.stderr).unwrap();
    let envelope: Value = serde_json::from_str(stderr)
        .unwrap_or_else(|error| panic!("stderr JSON: {error}: {stderr}"));
    assert_eq!(envelope["kind"], "land_checkpoint_rolled_back");
    assert_eq!(
        json(fixture.path(), &["--output", "json", "status"])["current_state"],
        before_state,
        "auto-undo must restore the pre-land Heddle tip"
    );
    assert_eq!(
        git_stdout(fixture.path(), &["rev-parse", "HEAD"]),
        before_git
    );
    assert_eq!(git_ref_snapshot(fixture.path()), before_refs);
}

#[test]
fn git_overlay_matrix_integrated_marker_write_failure_uses_committed_batch_for_rollback() {
    let fixture = GitOverlayFixture::imported_main()
        .with_ready_materialized_thread("feature/integrated-marker-rollback");
    let before_state =
        json(fixture.path(), &["--output", "json", "status"])["current_state"].clone();
    let before_git = git_stdout(fixture.path(), &["rev-parse", "HEAD"]);

    let land = heddle_output_with_env(
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/integrated-marker-rollback",
        ],
        Some(fixture.path()),
        &[(
            "HEDDLE_FAULT_INJECT",
            "land_before_integrated_marker_update",
        )],
    )
    .expect("invoke land with integrated marker fault injection");
    assert!(!land.status.success());
    let envelope: Value = serde_json::from_slice(&land.stderr).expect("typed rollback error");
    assert_eq!(
        envelope["kind"], "land_checkpoint_rolled_back",
        "{envelope}"
    );
    assert_eq!(
        json(fixture.path(), &["--output", "json", "status"])["current_state"],
        before_state,
        "rollback must use the committed integration transaction even while the durable marker is still Prepared"
    );
    assert_eq!(
        git_stdout(fixture.path(), &["rev-parse", "HEAD"]),
        before_git
    );
    assert!(
        !fixture.path().join(".heddle/incomplete-land.json").exists(),
        "successful rollback must consume the prepared marker"
    );
}

#[test]
fn git_overlay_matrix_prepared_marker_recovers_unrecorded_owned_collapse() {
    let thread = "feature/collapse-marker-recovery";
    let fixture = GitOverlayFixture::imported_main().with_ready_materialized_thread(thread);
    std::fs::write(fixture.ready_thread_path().join("second.txt"), "second\n").unwrap();
    let second = json(
        fixture.ready_thread_path(),
        &["--output", "json", "ready", "-m", "second ready state"],
    );
    assert_eq!(second["status"], "completed", "{second}");

    let repo = repo::Repository::open(fixture.path()).unwrap();
    let before_source = repo
        .refs()
        .get_thread(&objects::object::ThreadName::new(thread))
        .unwrap()
        .expect("ready thread source");
    drop(repo);

    let failed = heddle_output_with_env(
        &["--output", "json", "land", "--thread", thread],
        Some(fixture.path()),
        &[("HEDDLE_FAULT_INJECT", "land_before_collapse_marker_update")],
    )
    .expect("invoke land with collapse marker fault injection");
    assert!(!failed.status.success());
    let marker_path = fixture.path().join(".heddle/incomplete-land.json");
    let marker: Value = serde_json::from_slice(&std::fs::read(&marker_path).unwrap()).unwrap();
    assert_eq!(marker["phase"], "prepared", "{marker}");
    assert!(marker["collapse_state"].is_null(), "{marker}");

    let repo = repo::Repository::open(fixture.path()).unwrap();
    let collapsed_source = repo
        .refs()
        .get_thread(&objects::object::ThreadName::new(thread))
        .unwrap()
        .expect("collapsed source");
    assert_ne!(collapsed_source, before_source);
    drop(repo);

    let recovery = heddle_output(&["--output", "json", "init"], Some(fixture.path())).unwrap();
    assert!(
        recovery.status.success(),
        "{}",
        String::from_utf8_lossy(&recovery.stderr)
    );
    let repo = repo::Repository::open(fixture.path()).unwrap();
    assert_eq!(
        repo.refs()
            .get_thread(&objects::object::ThreadName::new(thread))
            .unwrap(),
        Some(before_source),
        "recovery must infer and undo only the exact collapse from the recorded thread transition"
    );
    assert!(!marker_path.exists());
}

#[test]
fn git_overlay_matrix_land_prepared_journal_recovers_pre_publish_crash() {
    let fixture =
        GitOverlayFixture::imported_main().with_ready_materialized_thread("feature/land-prepared");
    let before_git = git_stdout(fixture.path(), &["rev-parse", "HEAD"]);
    let crashed = heddle_output_with_env(
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/land-prepared",
        ],
        Some(fixture.path()),
        &[(
            "HEDDLE_FAULT_INJECT",
            "land_after_integration_before_journal_update",
        )],
    )
    .expect("invoke land with pre-publication crash injection");
    assert!(!crashed.status.success());
    let marker = fixture.path().join(".heddle/incomplete-land.json");
    assert!(
        marker.is_file(),
        "prepared journal must precede integration"
    );
    assert_eq!(
        git_stdout(fixture.path(), &["rev-parse", "HEAD"]),
        before_git
    );

    let status = json(fixture.path(), &["--output", "json", "status"]);
    assert!(
        marker.is_file(),
        "observe-only status must retain the journal"
    );
    assert_eq!(status["coordination_status"], "blocked");

    let retry = heddle(
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/land-prepared",
        ],
        Some(fixture.path()),
    )
    .expect("retry should idempotently roll back the prepared phase and land");
    let retry: Value = serde_json::from_str(&retry).unwrap();
    assert_eq!(retry["status"], "landed", "{retry}");
    assert!(!marker.exists());
}

#[test]
fn git_overlay_matrix_automatic_land_recovers_each_transaction_boundary() {
    for fault in [
        "transactional_ff_after_worktree_before_commit",
        "transactional_ff_after_commit",
        "land_after_integration_before_journal_update",
    ] {
        let thread = format!("feature/auto-{fault}");
        let fixture = GitOverlayFixture::imported_main().with_ready_materialized_thread(&thread);
        let before_git = git_stdout(fixture.path(), &["rev-parse", "HEAD"]);
        let failed = heddle_output_with_env(
            &["--output", "json", "land", "--thread", &thread],
            Some(fixture.path()),
            &[("HEDDLE_FAULT_INJECT", fault)],
        )
        .unwrap_or_else(|error| panic!("invoke automatic land at {fault}: {error}"));
        assert!(!failed.status.success(), "fault {fault} must stop land");
        assert!(
            fixture.path().join(".heddle/incomplete-land.json").exists(),
            "fault {fault} must retain the prepared journal"
        );
        assert_eq!(
            git_stdout(fixture.path(), &["rev-parse", "HEAD"]),
            before_git,
            "Heddle integration at {fault} must not publish Git"
        );

        let retry = json(
            fixture.path(),
            &["--output", "json", "land", "--thread", &thread],
        );
        assert_eq!(retry["status"], "landed", "retry after {fault}: {retry}");
        assert!(
            !fixture.path().join(".heddle/incomplete-land.json").exists(),
            "retry after {fault} must consume the journal"
        );
    }
}

#[test]
fn git_overlay_matrix_manual_resolution_land_recovers_each_transaction_boundary() {
    for fault in [
        "transactional_ff_after_worktree_before_commit",
        "transactional_ff_after_commit",
        "land_after_integration_before_journal_update",
    ] {
        let thread = format!("feature/manual-{fault}");
        let fixture = GitOverlayFixture::imported_main().with_ready_materialized_thread(&thread);
        let resolved = json(
            fixture.path(),
            &["--output", "json", "thread", "resolve", &thread],
        );
        assert_eq!(
            resolved["status"], "completed",
            "fixture for {fault}: {resolved}"
        );
        let before_git = git_stdout(fixture.path(), &["rev-parse", "HEAD"]);
        let failed = heddle_output_with_env(
            &["--output", "json", "land", "--thread", &thread],
            Some(fixture.path()),
            &[("HEDDLE_FAULT_INJECT", fault)],
        )
        .unwrap_or_else(|error| panic!("invoke manual-resolution land at {fault}: {error}"));
        assert!(!failed.status.success(), "fault {fault} must stop land");
        assert!(
            fixture.path().join(".heddle/incomplete-land.json").exists(),
            "fault {fault} must retain the prepared journal"
        );
        assert_eq!(
            git_stdout(fixture.path(), &["rev-parse", "HEAD"]),
            before_git,
            "manual-resolution integration at {fault} must not publish Git"
        );

        let retry = json(
            fixture.path(),
            &["--output", "json", "land", "--thread", &thread],
        );
        assert_eq!(retry["status"], "landed", "retry after {fault}: {retry}");
        assert!(
            !fixture.path().join(".heddle/incomplete-land.json").exists(),
            "retry after {fault} must consume the journal"
        );
    }
}

#[test]
fn prepared_land_recovery_refuses_unrelated_git_divergence_at_mutation_chokepoint() {
    let fixture = GitOverlayFixture::imported_main()
        .with_ready_materialized_thread("feature/prepared-divergence");
    let crashed = heddle_output_with_env(
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/prepared-divergence",
        ],
        Some(fixture.path()),
        &[("HEDDLE_FAULT_INJECT", "land_after_prepared_journal")],
    )
    .unwrap();
    assert!(!crashed.status.success());
    let marker = fixture.path().join(".heddle/incomplete-land.json");
    assert!(marker.exists());

    std::fs::write(fixture.path().join("unrelated.txt"), "unrelated\n").unwrap();
    git_commit_all(fixture.path(), "unrelated external mutation");
    let unrelated_head = git_stdout(fixture.path(), &["rev-parse", "HEAD"]);
    let capture = heddle_output(
        &["capture", "-m", "must not consume unrelated work"],
        Some(fixture.path()),
    )
    .unwrap();

    assert!(!capture.status.success());
    assert_eq!(
        git_stdout(fixture.path(), &["rev-parse", "HEAD"]),
        unrelated_head
    );
    assert!(marker.exists(), "failed recovery must preserve its journal");
    assert!(
        String::from_utf8_lossy(&capture.stderr)
            .contains("refusing to infer or undo unrelated work"),
        "mutation chokepoint must fail closed: {}",
        String::from_utf8_lossy(&capture.stderr)
    );
}

#[test]
fn prepared_land_recovery_refuses_unowned_worktree_changes() {
    let fixture = GitOverlayFixture::imported_main()
        .with_ready_materialized_thread("feature/prepared-worktree-divergence");
    let crashed = heddle_output_with_env(
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/prepared-worktree-divergence",
        ],
        Some(fixture.path()),
        &[("HEDDLE_FAULT_INJECT", "land_after_prepared_journal")],
    )
    .unwrap();
    assert!(!crashed.status.success());
    let marker = fixture.path().join(".heddle/incomplete-land.json");
    assert!(marker.exists());

    let unrelated = fixture.path().join("unrelated.txt");
    std::fs::write(&unrelated, "unrelated and uncommitted\n").unwrap();
    let capture = heddle_output(
        &[
            "capture",
            "-m",
            "must not discard unrelated worktree changes",
        ],
        Some(fixture.path()),
    )
    .unwrap();

    assert!(!capture.status.success());
    assert_eq!(
        std::fs::read_to_string(&unrelated).unwrap(),
        "unrelated and uncommitted\n"
    );
    assert!(marker.exists(), "failed recovery must preserve its journal");
    assert!(
        String::from_utf8_lossy(&capture.stderr).contains("refusing to discard them"),
        "mutation chokepoint must fail closed: {}",
        String::from_utf8_lossy(&capture.stderr)
    );
}

#[test]
fn published_land_recovery_validates_branch_before_finalizing() {
    let fixture = GitOverlayFixture::imported_main()
        .with_ready_materialized_thread("feature/published-wrong-branch");
    let crashed = heddle_output_with_env(
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/published-wrong-branch",
        ],
        Some(fixture.path()),
        &[(
            "HEDDLE_FAULT_INJECT",
            "git_checkpoint_after_publish_before_phase",
        )],
    )
    .unwrap();
    assert!(!crashed.status.success());
    let published = git_stdout(fixture.path(), &["rev-parse", "HEAD"]);
    git(
        &["checkout", "-b", "unrelated-recovery-branch"],
        fixture.path(),
    );

    let ready = heddle_output(&["ready"], Some(fixture.path())).unwrap();
    assert!(!ready.status.success());
    assert_eq!(
        git_stdout(fixture.path(), &["rev-parse", "HEAD"]),
        published
    );
    assert!(
        fixture.path().join(".heddle/incomplete-land.json").exists(),
        "wrong-branch recovery must retain the journal"
    );
}

#[test]
fn git_overlay_matrix_rollback_started_phase_prevents_double_undo() {
    let fixture =
        GitOverlayFixture::imported_main().with_ready_materialized_thread("feature/rollback-phase");
    let before_state =
        json(fixture.path(), &["--output", "json", "status"])["current_state"].clone();
    let failed = heddle_output_with_env(
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/rollback-phase",
        ],
        Some(fixture.path()),
        &[(
            "HEDDLE_FAULT_INJECT",
            "git_checkpoint_before_write_through,land_after_rollback_before_journal_update",
        )],
    )
    .unwrap();
    assert!(!failed.status.success());
    let marker_path = fixture.path().join(".heddle/incomplete-land.json");
    let marker: Value = serde_json::from_slice(&std::fs::read(&marker_path).unwrap()).unwrap();
    assert_eq!(marker["phase"], "rollback_started", "{marker}");
    assert_eq!(
        json(fixture.path(), &["--output", "json", "status"])["current_state"],
        before_state,
        "first undo already restored the target"
    );

    let retry = heddle(
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/rollback-phase",
        ],
        Some(fixture.path()),
    )
    .expect("retry must finalize rollback without applying the undo twice");
    let retry: Value = serde_json::from_str(&retry).unwrap();
    assert_eq!(retry["status"], "landed", "{retry}");
    assert!(
        !fixture
            .path()
            .join(".heddle/state/git-checkpoint-intent.json")
            .exists(),
        "idempotent rollback recovery must remove its matching unpublished checkpoint intent"
    );
}

#[test]
fn git_overlay_matrix_land_recovers_checkpoint_published_before_crash() {
    let fixture =
        GitOverlayFixture::imported_main().with_ready_materialized_thread("feature/land-recover");
    let before_git = git_stdout(fixture.path(), &["rev-parse", "HEAD"]);

    let crashed = heddle_output_with_env(
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/land-recover",
        ],
        Some(fixture.path()),
        &[(
            "HEDDLE_FAULT_INJECT",
            "git_checkpoint_after_publish_before_phase",
        )],
    )
    .expect("invoke land with post-publish crash injection");
    assert!(!crashed.status.success());
    let marker = fixture.path().join(".heddle/incomplete-land.json");
    assert!(marker.is_file(), "crash must preserve the land journal");

    let after_publish = git_stdout(fixture.path(), &["rev-parse", "HEAD"]);
    assert_ne!(
        after_publish, before_git,
        "Git publication must have happened"
    );

    let nested = fixture.path().join("nested-status");
    std::fs::create_dir(&nested).unwrap();
    let status = json(&nested, &["--output", "json", "status"]);
    assert!(status["current_state"].as_str().is_some());
    assert!(
        status["blockers"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item
                .as_str()
                .is_some_and(|text| text.contains("recovery work pending")))),
        "observe-only status must report, not execute, durable recovery: {status}"
    );
    assert!(
        marker.exists(),
        "status must not mutate the recovery journal"
    );

    let _ = heddle(
        &[
            "--output",
            "json",
            "ready",
            "--thread",
            "feature/land-recover",
        ],
        Some(fixture.path()),
    );
    assert_eq!(
        git_stdout(fixture.path(), &["rev-parse", "HEAD"]),
        after_publish,
        "recovery must finalize the published checkpoint, not rewind Git"
    );
    assert!(
        !marker.exists(),
        "successful recovery must clear the journal"
    );
    assert!(
        !fixture
            .path()
            .join(".heddle/state/git-checkpoint-intent.json")
            .exists(),
        "recovery must finalize the durable checkpoint intent"
    );
}

#[test]
fn git_overlay_matrix_recovers_checkpoint_before_marker_oid_update() {
    let fixture = GitOverlayFixture::imported_main()
        .with_ready_materialized_thread("feature/checkpoint-marker-gap");
    let before_git = git_stdout(fixture.path(), &["rev-parse", "HEAD"]);
    let failed = heddle_output_with_env(
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/checkpoint-marker-gap",
        ],
        Some(fixture.path()),
        &[(
            "HEDDLE_FAULT_INJECT",
            "land_after_checkpoint_before_marker_update",
        )],
    )
    .expect("invoke land in checkpoint/marker crash window");
    assert!(!failed.status.success());
    let marker_path = fixture.path().join(".heddle/incomplete-land.json");
    let marker: Value = serde_json::from_slice(&std::fs::read(&marker_path).unwrap()).unwrap();
    assert!(marker["expected_git_oid"].is_null(), "{marker}");
    assert!(
        !fixture
            .path()
            .join(".heddle/state/git-checkpoint-intent.json")
            .exists(),
        "checkpoint intent is finalized before the injected failure"
    );
    let published = git_stdout(fixture.path(), &["rev-parse", "HEAD"]);
    assert_ne!(published, before_git);

    let retry = heddle_output(
        &[
            "--output",
            "json",
            "ready",
            "--thread",
            "feature/checkpoint-marker-gap",
        ],
        Some(fixture.path()),
    )
    .unwrap();
    assert!(
        retry.status.success(),
        "{}",
        String::from_utf8_lossy(&retry.stderr)
    );
    assert_eq!(
        git_stdout(fixture.path(), &["rev-parse", "HEAD"]),
        published
    );
    assert!(!marker_path.exists());
}

#[test]
fn git_overlay_matrix_old_state_checkpoint_does_not_complete_unpublished_land() {
    let fixture =
        GitOverlayFixture::imported_main().with_ready_materialized_thread("feature/land-old-map");
    let before_git = git_stdout(fixture.path(), &["rev-parse", "HEAD"]);
    let crashed = heddle_output_with_env(
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/land-old-map",
        ],
        Some(fixture.path()),
        &[(
            "HEDDLE_FAULT_INJECT",
            "git_checkpoint_after_intent_before_publish",
        )],
    )
    .expect("invoke land with pre-publish crash");
    assert!(!crashed.status.success());
    assert_eq!(
        git_stdout(fixture.path(), &["rev-parse", "HEAD"]),
        before_git
    );

    let marker_path = fixture.path().join(".heddle/incomplete-land.json");
    let marker: Value = serde_json::from_slice(&std::fs::read(&marker_path).unwrap()).unwrap();
    let merge_state = marker["merge_state"].as_str().unwrap();
    let repo = repo::Repository::open(fixture.path()).unwrap();
    let state = repo.resolve_state(merge_state).unwrap().unwrap();
    repo.record_git_checkpoint(&state, before_git.clone(), "pre-existing export")
        .unwrap();

    let _ = heddle(
        &[
            "--output",
            "json",
            "ready",
            "--thread",
            "feature/land-old-map",
        ],
        Some(fixture.path()),
    );
    assert_eq!(
        git_stdout(fixture.path(), &["rev-parse", "HEAD"]),
        before_git,
        "an old state mapping must not masquerade as this land's publication"
    );
    assert!(
        !marker_path.exists(),
        "ready should roll the incomplete land back"
    );
}

#[test]
fn git_overlay_matrix_recovery_is_idempotent_after_coalesce() {
    let fixture = GitOverlayFixture::imported_main()
        .with_ready_materialized_thread("feature/coalesced-recovery");
    let crashed = heddle_output_with_env(
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/coalesced-recovery",
        ],
        Some(fixture.path()),
        &[(
            "HEDDLE_FAULT_INJECT",
            "land_after_coalesce_before_journal_clear",
        )],
    )
    .unwrap();
    assert!(!crashed.status.success());
    let marker = fixture.path().join(".heddle/incomplete-land.json");
    assert!(marker.exists());
    let published = git_stdout(fixture.path(), &["rev-parse", "HEAD"]);
    let status = json(fixture.path(), &["--output", "json", "status"]);
    assert_eq!(status["coordination_status"], "blocked");
    assert!(marker.exists(), "status remains observe-only");

    let _ = heddle(
        &[
            "--output",
            "json",
            "ready",
            "--thread",
            "feature/coalesced-recovery",
        ],
        Some(fixture.path()),
    );
    assert_eq!(
        git_stdout(fixture.path(), &["rev-parse", "HEAD"]),
        published
    );
    assert!(
        !marker.exists(),
        "recovery must tolerate already-coalesced batches"
    );
}

#[test]
fn recovered_manual_land_clears_resolution_metadata_before_journal() {
    let thread = "feature/manual-recovery-cleanup";
    let fixture = GitOverlayFixture::imported_main().with_ready_materialized_thread(thread);
    let resolved = json(
        fixture.path(),
        &["--output", "json", "thread", "resolve", thread],
    );
    assert_eq!(resolved["status"], "completed", "{resolved}");
    let failed = heddle_output_with_env(
        &["--output", "json", "land", "--thread", thread],
        Some(fixture.path()),
        &[(
            "HEDDLE_FAULT_INJECT",
            "land_after_coalesce_before_journal_clear",
        )],
    )
    .unwrap();
    assert!(!failed.status.success());
    let marker = fixture.path().join(".heddle/incomplete-land.json");
    assert!(marker.exists());

    let retry = heddle_output(&["--output", "json", "init"], Some(fixture.path())).unwrap();
    assert!(
        retry.status.success(),
        "{}",
        String::from_utf8_lossy(&retry.stderr)
    );
    let repo = repo::Repository::open(fixture.path()).unwrap();
    let record = repo::ThreadManager::new(repo.heddle_dir())
        .load(thread)
        .unwrap()
        .expect("landed thread metadata");
    assert_eq!(
        record.integration_policy_result.manual_resolution_state,
        None
    );
    assert!(!record.integration_policy_result.conflicts_resolved_manually);
    assert!(!marker.exists());
}

#[test]
fn destination_init_does_not_recover_unrelated_cwd_repository() {
    let temp = TempDir::new().unwrap();
    let current = temp.path().join("current");
    let destination = temp.path().join("destination");
    std::fs::create_dir(&current).unwrap();
    std::fs::create_dir(&destination).unwrap();
    init_git_repo_with_branch(&current, "main");
    std::fs::write(current.join("README.md"), "current\n").unwrap();
    git_commit_all(&current, "current base");
    heddle(&["init"], Some(&current)).unwrap();
    let marker = current.join(".heddle/incomplete-land.json");
    std::fs::write(&marker, b"{}\n").unwrap();

    let destination_arg = destination.to_string_lossy().into_owned();
    let output = heddle_output(&["init", &destination_arg], Some(&current)).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(destination.join(".heddle").is_dir());
    assert!(
        marker.exists(),
        "destination init must not consume cwd recovery state"
    );

    let current_arg = current.to_string_lossy().into_owned();
    let targeted = heddle_output(&["init", &current_arg], Some(&destination)).unwrap();
    assert!(
        !targeted.status.success(),
        "positional init of an existing target must recover that target"
    );
    assert!(
        String::from_utf8_lossy(&targeted.stderr).contains("parse incomplete-land marker"),
        "target recovery error should remain actionable: {}",
        String::from_utf8_lossy(&targeted.stderr)
    );
}

#[test]
fn git_overlay_matrix_multi_peer_land_fast_forwards_git_tip() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("README.md"), "base\n").unwrap();
    git_commit_all(temp.path(), "base");
    heddle(&["init"], Some(temp.path())).expect("initialize Git Overlay");
    heddle(&["import", "git", "--ref", "main"], Some(temp.path())).expect("import main");

    let alpha = temp.path().with_extension("alpha");
    let beta = temp.path().with_extension("beta");
    json(
        temp.path(),
        &[
            "--output",
            "json",
            "start",
            "alpha",
            "--path",
            alpha.to_str().unwrap(),
        ],
    );
    json(
        temp.path(),
        &[
            "--output",
            "json",
            "start",
            "beta",
            "--path",
            beta.to_str().unwrap(),
        ],
    );
    std::fs::write(alpha.join("alpha.txt"), "alpha\n").unwrap();
    json(&alpha, &["--output", "json", "capture", "-m", "alpha edit"]);
    std::fs::write(beta.join("beta.txt"), "beta\n").unwrap();
    json(&beta, &["--output", "json", "capture", "-m", "beta edit"]);

    let alpha_land = json(
        temp.path(),
        &["--output", "json", "land", "--thread", "alpha"],
    );
    assert_eq!(alpha_land["status"], "landed", "{alpha_land}");
    let after_alpha = git_stdout(temp.path(), &["rev-parse", "HEAD"]);
    assert_eq!(alpha_land["checkpointed"], true, "{alpha_land}");

    let beta_land = json(
        temp.path(),
        &["--output", "json", "land", "--thread", "beta"],
    );
    assert_eq!(beta_land["status"], "landed", "{beta_land}");
    let after_beta = git_stdout(temp.path(), &["rev-parse", "HEAD"]);
    assert_ne!(after_beta, after_alpha);
    let is_ancestor = Command::new("git")
        .args(["merge-base", "--is-ancestor", &after_alpha, &after_beta])
        .current_dir(temp.path())
        .status()
        .expect("git merge-base --is-ancestor");
    assert!(is_ancestor.success());
    let tree = git_stdout(temp.path(), &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(tree.lines().any(|line| line == "alpha.txt"));
    assert!(tree.lines().any(|line| line == "beta.txt"));
}

#[test]
fn git_overlay_matrix_land_threads_flag_lands_peers_in_order() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("README.md"), "base\n").unwrap();
    git_commit_all(temp.path(), "base");
    heddle(&["init"], Some(temp.path())).expect("initialize Git Overlay");
    heddle(&["import", "git", "--ref", "main"], Some(temp.path())).expect("import main");

    let alpha = temp.path().with_extension("alpha-mt");
    let beta = temp.path().with_extension("beta-mt");
    for (name, path) in [("alpha", &alpha), ("beta", &beta)] {
        json(
            temp.path(),
            &[
                "--output",
                "json",
                "start",
                name,
                "--path",
                path.to_str().unwrap(),
            ],
        );
    }
    std::fs::write(alpha.join("a.txt"), "a\n").unwrap();
    json(&alpha, &["--output", "json", "capture", "-m", "a"]);
    std::fs::write(beta.join("b.txt"), "b\n").unwrap();
    json(&beta, &["--output", "json", "capture", "-m", "b"]);

    let out = heddle(
        &["--output", "json", "land", "--threads", "alpha,beta"],
        Some(temp.path()),
    )
    .expect("land --threads alpha,beta");
    let batch: Value = serde_json::from_str(out.trim())
        .unwrap_or_else(|error| panic!("expected one land_batch JSON: {error}: {out}"));
    assert_eq!(batch["output_kind"], "land_batch", "{batch}");
    assert_eq!(batch["status"], "landed", "{batch}");
    assert_eq!(batch["peers"].as_array().map(Vec::len), Some(2));
    let tree = git_stdout(temp.path(), &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(tree.lines().any(|line| line == "a.txt"));
    assert!(tree.lines().any(|line| line == "b.txt"));
}

#[test]
fn git_overlay_matrix_land_threads_reports_failed_peer_in_batch() {
    let fixture =
        GitOverlayFixture::imported_main().with_ready_materialized_thread("feature/batch-fail");
    let out = heddle_output_with_env(
        &[
            "--output",
            "json",
            "land",
            "--threads",
            "feature/batch-fail",
        ],
        Some(fixture.path()),
        &[("HEDDLE_FAULT_INJECT", "git_checkpoint_before_write_through")],
    )
    .expect("invoke batch land with checkpoint failure");
    assert!(!out.status.success());
    assert!(
        out.stderr.is_empty(),
        "batch must not emit a second envelope: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let batch: Value = serde_json::from_slice(&out.stdout).expect("single batch JSON on stdout");
    assert_eq!(batch["output_kind"], "land_batch", "{batch}");
    assert_eq!(batch["status"], "blocked", "{batch}");
    assert_eq!(batch["stopped_at"], "feature/batch-fail", "{batch}");
    assert_eq!(
        batch["git_head"],
        git_stdout(fixture.path(), &["rev-parse", "HEAD"]),
        "batch envelope Git state must come from the resolved target repository"
    );
    assert!(
        batch["verification"].is_object(),
        "batch envelope trust must come from the resolved target repository: {batch}"
    );
    assert_eq!(batch["peers"].as_array().map(Vec::len), Some(1));
    assert_eq!(batch["peers"][0]["thread"], "feature/batch-fail");
    assert_eq!(batch["peers"][0]["status"], "blocked");
    assert_eq!(
        batch["peers"][0]["primary_command"],
        "heddle land --thread feature/batch-fail"
    );
    assert_eq!(
        batch["peers"][0]["recovery_commands"],
        serde_json::json!(["heddle verify", "heddle land --thread feature/batch-fail"])
    );
    assert_eq!(
        batch["recommended_action"],
        "heddle land --thread feature/batch-fail"
    );
    assert!(
        batch["peers"][0]["message"]
            .as_str()
            .is_some_and(|message| message.contains("rolled back")),
        "{batch}"
    );
}

#[test]
fn git_overlay_matrix_land_reports_sibling_enumeration_failure() {
    let fixture = GitOverlayFixture::imported_main()
        .with_ready_materialized_thread("feature/sibling-enumeration");
    let out = heddle_output_with_env(
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/sibling-enumeration",
        ],
        Some(fixture.path()),
        &[("HEDDLE_FAULT_INJECT", "land_sibling_enumeration")],
    )
    .unwrap();
    assert!(out.status.success());
    let land: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(land["status"], "landed", "{land}");
    assert_eq!(
        land["siblings_restack_failed"].as_array().map(Vec::len),
        Some(1),
        "enumeration failure must remain structured: {land}"
    );
    assert!(
        land["warnings"]
            .as_array()
            .is_some_and(|warnings| !warnings.is_empty()),
        "enumeration failure must be loud: {land}"
    );
}

#[test]
fn git_overlay_matrix_human_land_batch_prints_peer_warnings_and_restack_failures() {
    let fixture = GitOverlayFixture::imported_main()
        .with_ready_materialized_thread("feature/human-batch-warning");
    let out = heddle_output_with_env(
        &[
            "--output",
            "text",
            "land",
            "--threads",
            "feature/human-batch-warning",
        ],
        Some(fixture.path()),
        &[("HEDDLE_FAULT_INJECT", "land_sibling_enumeration")],
    )
    .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("warning:"),
        "peer warnings must be visible: {stdout}"
    );
    assert!(
        stdout.contains("restack failed:"),
        "each peer restack failure must be visible: {stdout}"
    );
}

#[test]
fn git_overlay_matrix_manual_git_merge_commit_after_bootstrap_commands() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("shared.txt"), "base\n").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(
        &["import", "git", "--ref", "feature/drop-in"],
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
    assert_eq!(status["recommended_action"], Value::Null);
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
fn git_overlay_matrix_imported_branch_git_only_advance_reappears_in_import_hint() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed branch");

    git(&["checkout", "-b", "support/alpha"], temp.path());
    std::fs::write(temp.path().join("alpha.txt"), "alpha one\n").unwrap();
    git_commit_all(temp.path(), "alpha one");
    git(&["checkout", "feature/drop-in"], temp.path());

    let import_output = heddle(&["import", "git", "--path", "."], Some(temp.path())).unwrap();
    assert!(
        import_output.contains("branches") || import_output.contains("\"branches_synced\""),
        "Git import should report branch sync activity: {import_output}"
    );

    let threads_after_import = json(temp.path(), &["thread", "list", "--output", "json"]);
    assert!(
        threads_after_import["threads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|thread| thread["name"] == "support/alpha"),
        "thread list should include imported branch after Git import: {threads_after_import}"
    );

    git(&["checkout", "support/alpha"], temp.path());
    std::fs::write(temp.path().join("alpha.txt"), "alpha two\n").unwrap();
    git_commit_all(temp.path(), "alpha two");
    git(&["checkout", "feature/drop-in"], temp.path());

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_no_legacy_verification_sidecars(&status);

    let bridge = json(temp.path(), &["status", "--output", "json"]);
    assert_no_legacy_verification_sidecars(&bridge);
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

    let _ = heddle(&["import", "git", "--path", "."], Some(temp.path())).unwrap();

    git(&["branch", "-D", "support/reborn"], temp.path());
    git(&["checkout", "-b", "support/reborn"], temp.path());
    std::fs::write(temp.path().join("reborn.txt"), "second life\n").unwrap();
    git_commit_all(temp.path(), "second reborn");
    git(&["checkout", "feature/drop-in"], temp.path());

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_no_legacy_verification_sidecars(&status);

    let bridge_again = json(temp.path(), &["status", "--output", "json"]);
    assert_no_legacy_verification_sidecars(&bridge_again);
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
    initialize_git_overlay(rebase_repo.path());

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

    let doctor = json(rebase_repo.path(), &["doctor", "--output", "json"]);
    assert_eq!(doctor["repository_capability"], "git-overlay");

    let worktree = json(rebase_repo.path(), &["status", "--output", "json"]);
    assert_eq!(worktree["repository_capability"], "git-overlay");

    git(&["rebase", "--abort"], rebase_repo.path());

    let cherry_repo = TempDir::new().unwrap();
    init_git_repo_with_branch(cherry_repo.path(), "feature/drop-in");
    std::fs::write(cherry_repo.path().join("conflict.txt"), "base\n").unwrap();
    git_commit_all(cherry_repo.path(), "seed branch");
    initialize_git_overlay(cherry_repo.path());

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
        &["import", "git", "--ref", "feature/drop-in"],
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
        cherry_status["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "conflict.txt")
            || cherry_status["changes"]["modified"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "conflict.txt"),
        "status should stay coherent during cherry-pick conflict: {cherry_status}"
    );

    let cherry_show = json(cherry_repo.path(), &["show", "HEAD", "--output", "json"]);
    assert!(cherry_show["state_id"].as_str().is_some());

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
    assert_eq!(envelope["primary_command"], raw_git_preservation_action());
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("Git-compatible tool that started it")
                && hint.contains(raw_git_preservation_action())
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

    initialize_git_overlay(&worktree_path);
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

    let workspace = json(&worktree_path, &["status", "--output", "json"]);
    assert_eq!(workspace["thread"], "support/native-worktree");

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

    let workspace = json(temp.path(), &["status", "--output", "json"]);
    assert_eq!(workspace["thread"], "feature/renamed-current");
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

    let _ = heddle(&["import", "git", "--path", "."], Some(temp.path())).unwrap();

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

    let status = json(temp.path(), &["status", "--output", "json"]);
    assert_no_legacy_verification_sidecars(&status);
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
        raw_git_preservation_action()
    );
    let doctor = json(rebase_repo.path(), &["doctor", "--output", "json"]);
    assert_eq!(doctor["operation"]["kind"], "rebase");
    let workspace = json(rebase_repo.path(), &["status", "--output", "json"]);
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
        raw_git_preservation_action()
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
    assert_eq!(bisect_status["operation"]["next_action"], "heddle abort");
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
    initialize_git_overlay(temp.path());

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
    initialize_git_overlay(&worktree_path);
    let worktree_status = json(&worktree_path, &["status", "--verbose", "--output", "json"]);
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
    let switched = json(&worktree_path, &["status", "--output", "json"]);
    assert_eq!(switched["thread"], "support/renamed-switch");

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

    let root_status = json(temp.path(), &["status", "--verbose", "--output", "json"]);
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
