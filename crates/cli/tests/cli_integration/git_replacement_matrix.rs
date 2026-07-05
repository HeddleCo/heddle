// SPDX-License-Identifier: Apache-2.0
use super::*;

fn heddle_without_git(args: &[&str], cwd: &std::path::Path) -> Result<String, String> {
    let output = heddle_output_with_env(args, Some(cwd), &[("PATH", "")])?;
    let stdout = str::from_utf8(&output.stdout).unwrap_or("").to_string();
    let stderr = str::from_utf8(&output.stderr).unwrap_or("").to_string();
    if output.status.success() {
        Ok(stdout)
    } else {
        Err(format!(
            "Exit code: {:?}\nstdout: {}\nstderr: {}",
            output.status.code(),
            stdout,
            stderr
        ))
    }
}

fn heddle_output_without_git(args: &[&str], cwd: &std::path::Path) -> Output {
    heddle_output_with_env(args, Some(cwd), &[("PATH", ""), ("NO_COLOR", "1")])
        .expect("invoke heddle without git")
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

fn assert_clean_json_without_git(args: &[&str], cwd: &std::path::Path) -> Value {
    let output = heddle_output_without_git(args, cwd);
    let stdout = str::from_utf8(&output.stdout).unwrap_or("");
    let stderr = str::from_utf8(&output.stderr).unwrap_or("");
    assert!(
        output.status.success(),
        "{args:?} should succeed without git on PATH; stdout={stdout} stderr={stderr}"
    );
    assert!(
        stderr.is_empty(),
        "{args:?} JSON success must not write warnings/prose to stderr: {stderr}"
    );
    let value: Value = serde_json::from_str(stdout)
        .unwrap_or_else(|err| panic!("{args:?} should emit parseable JSON: {err}: {stdout}"));
    if args.contains(&"verify") {
        return verify_state_for_assertions(value);
    }
    inject_post_verification_without_git(cwd, value)
}

/// Mutation `--output json` replies no longer embed `verification`
/// (the verification-claim gate still consults it in-memory, but it
/// is omitted from the wire). This helper grafts the proof back onto
/// the returned value for test ergonomics by invoking
/// `heddle verify --output json` after the original call.
fn inject_post_verification_without_git(cwd: &std::path::Path, mut value: Value) -> Value {
    let obj = match value.as_object_mut() {
        Some(obj) => obj,
        None => return value,
    };
    if obj.contains_key("verification") {
        return value;
    }
    let verify_out = heddle_output_without_git(&["--output", "json", "verify"], cwd);
    let stream = if !verify_out.status.success() {
        verify_out.stderr
    } else {
        verify_out.stdout
    };
    let text = str::from_utf8(&stream).unwrap_or("");
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

fn assert_verify_failed_json_without_git(args: &[&str], cwd: &std::path::Path) -> Value {
    let output = heddle_output_without_git(args, cwd);
    let stdout = str::from_utf8(&output.stdout).unwrap_or("");
    let stderr = str::from_utf8(&output.stderr).unwrap_or("");
    assert!(
        !output.status.success(),
        "{args:?} should be a strict verify failure without git on PATH; stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.is_empty(),
        "{args:?} JSON failure must not write a second JSON value to stdout: {stdout}"
    );
    let envelope: Value = serde_json::from_str(stderr).unwrap_or_else(|err| {
        panic!("{args:?} should emit parseable JSON envelope: {err}: {stderr}")
    });
    assert_eq!(envelope["kind"], "verify_failed", "{envelope}");
    envelope["verification"].clone()
}

fn configure_repo_local_git_identity(path: &std::path::Path) {
    let config = path.join(".git").join("config");
    let mut contents = std::fs::read_to_string(&config).unwrap_or_default();
    if !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str("[user]\n\tname = Heddle Test\n\temail = heddle@example.com\n");
    std::fs::write(config, contents).expect("write repo-local git identity");
}

fn git_ok(args: &[&str], cwd: &std::path::Path) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {:?} failed\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_stdout(args: &[&str], cwd: &std::path::Path) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {:?} failed\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn seed_bare_git_repo(path: &std::path::Path) -> ObjectId {
    let repo = SleyRepository::init_bare(path).expect("init bare git repo");
    let commit = git_commit_with_tree(
        &repo,
        Some("refs/heads/main"),
        git_empty_tree_oid(&repo),
        "seed",
        &[],
    );
    git_set_reference(&repo, "HEAD", commit);
    commit
}

fn git_tree_with_file(repo: &SleyRepository, path: &str, content: &[u8]) -> ObjectId {
    let blob = repo.write_blob(content).expect("write git blob");
    let empty = git_empty_tree_oid(repo);
    let mut editor = repo.edit_tree(&empty).expect("edit git tree");
    editor.upsert(path, EntryKind::Blob, blob);
    repo.write_tree(editor).expect("write git tree")
}

fn git_head_oid(path: &std::path::Path) -> String {
    open_git(path)
        .expect("open git repo")
        .head_id()
        .expect("resolve HEAD")
        .to_string()
}

#[test]
fn git_replacement_matrix_fresh_git_read_commands_without_git_on_path() {
    let temp = TempDir::new().unwrap();
    SleyRepository::init(temp.path()).expect("init git worktree");
    std::fs::write(temp.path().join("pending.txt"), "pending\n").unwrap();

    let status = heddle_without_git(&["status", "--output", "json"], temp.path()).unwrap();
    let parsed: Value = serde_json::from_str(&status).expect("status should parse");
    assert_eq!(parsed["repository_capability"], "plain-git");
    assert_eq!(parsed["heddle_initialized"], false);
    assert_eq!(parsed["recommended_action"], "heddle init");
    assert!(
        parsed["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "pending.txt"),
        "fresh git status should report pending work without git on PATH: {status}"
    );

    for args in [
        &["doctor", "--output", "json"][..],
        &["doctor", "--output", "json"],
        &["status", "--output", "json"],
        &["thread", "list", "--output", "json"],
        &["status", "--output", "json"],
    ] {
        let stdout = heddle_without_git(args, temp.path())
            .unwrap_or_else(|err| panic!("{args:?} should not require git on PATH: {err}"));
        let parsed: Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|err| panic!("{args:?} should emit JSON: {err}: {stdout}"));
        if parsed.get("repository_capability").is_some() {
            assert_eq!(
                parsed["repository_capability"], "plain-git",
                "{args:?} should report the observe-only plain-Git mode: {stdout}"
            );
        }
        if parsed.get("repository_mode").is_some() {
            assert_eq!(
                parsed["repository_mode"], "plain-git",
                "{args:?} should report the observe-only plain-Git mode: {stdout}"
            );
        }
        assert!(
            !temp.path().join(".heddle").exists(),
            "{args:?} must not initialize Heddle metadata in a plain Git repo"
        );
    }
    let verify =
        assert_verify_failed_json_without_git(&["verify", "--output", "json"], temp.path());
    assert_eq!(verify["repository_mode"], "plain-git");
    assert_eq!(verify["status"], "needs_init");
    assert_eq!(verify["recommended_action"], "heddle init");
    assert!(
        !temp.path().join(".heddle").exists(),
        "verify failure must remain observe-only in a plain Git repo"
    );

    let catalog = assert_clean_json_without_git(&["help", "--output", "json"], temp.path());
    let commands = catalog["commands"]
        .as_array()
        .expect("command catalog should expose commands");
    assert!(
        commands
            .iter()
            .all(|command| command["requires_git_executable"] == false),
        "command catalog must make the no-Git-runtime contract machine-readable: {catalog}"
    );
    assert!(
        catalog["recommended_action_placeholders"]
            .as_array()
            .expect("placeholder registry should be cataloged")
            .iter()
            .all(|action| !action
                .as_str()
                .is_some_and(|action| action.starts_with("git "))),
        "no-Git runtime catalog must not advertise raw Git recovery placeholders: {catalog}"
    );
    assert!(
        !temp.path().join(".heddle").exists(),
        "help catalog must stay observe-only in a plain Git repo"
    );

    let committed = TempDir::new().unwrap();
    let repo = SleyRepository::init(committed.path()).expect("init committed git worktree");
    std::fs::write(
        committed.path().join(".git").join("HEAD"),
        "ref: refs/heads/main\n",
    )
    .expect("point HEAD at main");
    std::fs::write(committed.path().join("tracked.txt"), "tracked\n").unwrap();
    let tree = git_tree_with_file(&repo, "tracked.txt", b"tracked\n");
    git_commit_with_tree(&repo, Some("refs/heads/main"), tree, "seed", &[]);

    let verify =
        assert_verify_failed_json_without_git(&["verify", "--output", "json"], committed.path());
    assert_eq!(verify["repository_mode"], "plain-git");
    assert_eq!(verify["status"], "needs_init");
    assert_eq!(verify["recommended_action"], "heddle init");
    assert_eq!(
        verify["recommended_action_template"]["argv_template"],
        heddle_argv_json(["init"]),
        "machine argv must replay the same Heddle binary even when PATH cannot resolve `heddle`: {verify}"
    );
    assert_eq!(
        verify["checks"][1]["recommended_action_template"]["argv_template"],
        heddle_argv_json(["init"]),
        "per-check argv must also be hermetic for no-PATH agents: {verify}"
    );
    assert!(
        !committed.path().join(".heddle").exists(),
        "verify in a committed plain Git repo must stay observe-only"
    );
}

#[test]
fn git_replacement_matrix_shallow_import_refuses_without_raw_git_advice() {
    let temp = TempDir::new().unwrap();
    let git = SleyRepository::init(temp.path()).expect("init git worktree");
    std::fs::write(
        temp.path().join(".git").join("HEAD"),
        "ref: refs/heads/main\n",
    )
    .expect("point HEAD at main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    let tree = git_tree_with_file(&git, "tracked.txt", b"tracked\n");
    let commit = git_commit_with_tree(&git, Some("refs/heads/main"), tree, "seed", &[]);
    std::fs::write(
        temp.path().join(".git").join("shallow"),
        format!("{commit}\n"),
    )
    .expect("mark repo shallow");

    assert_clean_json_without_git(&["--output", "json", "init"], temp.path());

    let output = heddle_output_without_git(
        &["--output", "json", "import", "git", "--ref", "main"],
        temp.path(),
    );
    let stdout = str::from_utf8(&output.stdout).unwrap_or("");
    let stderr = str::from_utf8(&output.stderr).unwrap_or("");
    assert!(
        !output.status.success(),
        "shallow import should fail closed; stdout={stdout} stderr={stderr}"
    );
    assert!(
        !stderr.contains("git -C") && !stderr.contains("fetch --unshallow"),
        "shallow import recovery must not require the git executable: {stderr}"
    );
    let envelope: Value =
        serde_json::from_str(stderr).expect("shallow import should emit JSON envelope");
    assert_eq!(envelope["kind"], "git_overlay_shallow_clone");
    assert_eq!(
        envelope["primary_command"],
        "heddle clone <remote> <fresh-path>"
    );
    assert_eq!(
        envelope["recovery_commands"],
        serde_json::json!([
            "heddle clone <remote> <fresh-path>",
            "heddle import git --path <full-git-repo> --ref <ref>"
        ])
    );
    assert_eq!(
        envelope["recovery_action_templates"][0]["action"],
        "heddle clone <remote> <fresh-path>"
    );
    assert_eq!(
        envelope["recovery_action_templates"][1]["action"],
        "heddle import git --path <full-git-repo> --ref <ref>"
    );
}

#[test]
fn git_replacement_matrix_raw_git_operation_handoff_without_git_on_path() {
    let temp = TempDir::new().unwrap();
    let git = SleyRepository::init(temp.path()).expect("init git worktree");
    configure_repo_local_git_identity(temp.path());
    std::fs::write(
        temp.path().join(".git").join("HEAD"),
        "ref: refs/heads/main\n",
    )
    .expect("point HEAD at main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    let tree = git_tree_with_file(&git, "tracked.txt", b"tracked\n");
    let commit = git_commit_with_tree(&git, Some("refs/heads/main"), tree, "seed", &[]);

    let adopt =
        assert_clean_json_without_git(&["--output", "json", "adopt", "--ref", "main"], temp.path());
    assert_ne!(adopt["verification"]["status"], "needs_import");

    std::fs::write(
        temp.path().join(".git").join("MERGE_HEAD"),
        commit.to_string(),
    )
    .expect("simulate externally-started raw Git merge");
    std::fs::write(temp.path().join("tracked.txt"), "raw git operation work\n").unwrap();

    let status = assert_clean_json_without_git(&["--output", "json", "status"], temp.path());
    assert_eq!(status["operation"]["scope"], "git");
    assert_eq!(status["operation"]["kind"], "merge");
    assert_eq!(status["recommended_action"], "heddle status");

    let continued_output =
        heddle_output_without_git(&["--output", "json", "continue"], temp.path());
    let continued_stdout = str::from_utf8(&continued_output.stdout).unwrap_or("");
    let continued_stderr = str::from_utf8(&continued_output.stderr).unwrap_or("");
    assert!(
        !continued_output.status.success(),
        "raw Git continue handoff should refuse without git on PATH; stdout={continued_stdout} stderr={continued_stderr}"
    );
    assert!(
        continued_stderr.is_empty(),
        "JSON handoff refusal should emit a single machine value on stdout: {continued_stderr}"
    );
    let continued: Value = serde_json::from_str(continued_stdout).unwrap_or_else(|err| {
        panic!("continue refusal should emit parseable JSON: {err}: {continued_stdout}")
    });
    assert_eq!(continued["status"], "blocked");
    assert_eq!(continued["action"], "merge");
    assert!(
        continued["message"]
            .as_str()
            .is_some_and(|message| message.contains("no-git runtime")),
        "continue handoff should explain the no-git contract: {continued}"
    );
    assert_eq!(continued["recommended_action"], "heddle status");
}

#[test]
fn git_replacement_matrix_native_repo_read_commands_without_git_on_path() {
    let temp = TempDir::new().unwrap();

    let init = assert_clean_json_without_git(&["--output", "json", "init"], temp.path());
    assert_eq!(init["repository_mode"], "native-heddle", "{init}");
    assert_eq!(init["git_detected"], false, "{init}");

    std::fs::write(temp.path().join("story.txt"), "one\n").unwrap();
    let first = assert_clean_json_without_git(
        &[
            "--output",
            "json",
            "capture",
            "-m",
            "native seed",
            "--confidence",
            "0.9",
        ],
        temp.path(),
    );
    let first_id = first["change_id"]
        .as_str()
        .expect("first capture id")
        .to_string();
    assert!(
        first_id.starts_with("hd-"),
        "native capture should produce Heddle state ids: {first}"
    );

    std::fs::write(temp.path().join("story.txt"), "one\ntwo\n").unwrap();

    let diff_text = heddle_output_without_git(&["--output", "text", "diff"], temp.path());
    let diff_stdout = str::from_utf8(&diff_text.stdout).unwrap_or("");
    let diff_stderr = str::from_utf8(&diff_text.stderr).unwrap_or("");
    assert!(
        diff_text.status.success(),
        "native diff should not require git on PATH; stdout={diff_stdout} stderr={diff_stderr}"
    );
    assert!(
        diff_stderr.is_empty(),
        "native diff success should keep stderr quiet: {diff_stderr}"
    );
    assert!(
        diff_stdout.contains("+two"),
        "native diff should render worktree additions: {diff_stdout}"
    );

    let second = assert_clean_json_without_git(
        &[
            "--output",
            "json",
            "capture",
            "-m",
            "native update",
            "--confidence",
            "0.9",
        ],
        temp.path(),
    );
    let second_id = second["change_id"]
        .as_str()
        .expect("second capture id")
        .to_string();

    let state_show =
        assert_clean_json_without_git(&["--output", "json", "show", "HEAD"], temp.path());
    assert_eq!(
        state_show["repository_capability"], "native-heddle",
        "{state_show}"
    );
    assert_eq!(
        state_show["change_id"], second_id,
        "show HEAD should inspect the latest native Heddle state: {state_show}"
    );

    let state_inspect =
        assert_clean_json_without_git(&["--output", "json", "show", &first_id], temp.path());
    assert_eq!(
        state_inspect["change_id"], first_id,
        "show <state> should route to native state show without git: {state_inspect}"
    );

    let thread_inspect =
        assert_clean_json_without_git(&["--output", "json", "thread", "show", "main"], temp.path());
    assert_eq!(
        thread_inspect["output_kind"], "thread_show",
        "{thread_inspect}"
    );
    assert_eq!(
        thread_inspect["current_state"], second_id,
        "thread show <thread> should route to thread show without git: {thread_inspect}"
    );

    let state_diff = assert_clean_json_without_git(
        &["--output", "json", "diff", &first_id, &second_id],
        temp.path(),
    );
    assert_eq!(state_diff["output_kind"], "diff", "{state_diff}");
    assert_eq!(state_diff["changed_path_count"], 1, "{state_diff}");
    assert_eq!(
        state_diff["changes"][0]["path"], "story.txt",
        "{state_diff}"
    );

    for args in [
        &["--output", "json", "status"][..],
        &["--output", "json", "log"],
        &["--output", "json", "thread", "show", "main"],
        &["--output", "json", "status"],
    ] {
        let parsed = assert_clean_json_without_git(args, temp.path());
        assert!(
            parsed.is_object(),
            "{args:?} should stay machine-readable without git on PATH: {parsed}"
        );
    }
}

#[test]
fn git_replacement_matrix_everyday_save_read_machine_streams_without_git_on_path() {
    let temp = TempDir::new().unwrap();
    SleyRepository::init(temp.path()).expect("init git worktree");
    configure_repo_local_git_identity(temp.path());

    let init = assert_clean_json_without_git(&["--output", "json", "init"], temp.path());
    assert!(
        init["path"].as_str().unwrap_or("").ends_with(".heddle"),
        "init JSON should report the sidecar path: {init}"
    );

    std::fs::write(temp.path().join("seed.txt"), "seed\n").unwrap();
    let status = assert_clean_json_without_git(&["--output", "json", "status"], temp.path());
    assert_eq!(status["repository_capability"], "git-overlay");
    assert!(
        status["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "seed.txt"),
        "status should report the dirty path: {status}"
    );

    let capture = assert_clean_json_without_git(
        &[
            "--output",
            "json",
            "capture",
            "-m",
            "seed",
            "--confidence",
            "0.9",
        ],
        temp.path(),
    );
    assert!(
        capture["change_id"]
            .as_str()
            .unwrap_or("")
            .starts_with("hd-")
    );

    let checkpoint = assert_clean_json_without_git(
        &["--output", "json", "checkpoint", "-m", "checkpoint"],
        temp.path(),
    );
    assert_eq!(checkpoint["capability"], "git-overlay");
    assert!(checkpoint["git_commit"].as_str().unwrap_or("").len() >= 7);

    for args in [
        &["--output", "json", "log"][..],
        &["--output", "json", "show", "HEAD"],
        &["--output", "json", "doctor"],
        &["--output", "json", "ready"],
    ] {
        let parsed = assert_clean_json_without_git(args, temp.path());
        assert!(
            parsed.is_object(),
            "{args:?} should emit a JSON object for automation: {parsed}"
        );
    }

    std::fs::write(temp.path().join("seed.txt"), "seed\nchange\n").unwrap();
    let diff = heddle_output_without_git(&["--output", "text", "diff"], temp.path());
    let stdout = str::from_utf8(&diff.stdout).unwrap_or("");
    let stderr = str::from_utf8(&diff.stderr).unwrap_or("");
    assert!(diff.status.success(), "diff should succeed: {stderr}");
    assert!(
        stderr.is_empty(),
        "diff text success should keep stderr quiet: {stderr}"
    );
    assert!(
        stdout.contains("+change"),
        "diff should show the changed line: {stdout}"
    );
    assert!(
        !stdout.contains('\u{1b}'),
        "NO_COLOR=1 text output must not contain ANSI escapes: {stdout:?}"
    );
}

#[test]
fn git_replacement_matrix_clone_status_capture_push_without_git_on_path() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    let original_tip = seed_bare_git_repo(&origin);

    let clone = heddle_without_git(
        &[
            "--output",
            "json",
            "clone",
            origin.to_str().expect("origin path should be utf8"),
            work.to_str().expect("work path should be utf8"),
        ],
        temp.path(),
    )
    .unwrap();
    configure_repo_local_git_identity(&work);
    assert!(
        clone.contains("Imported 1 Git commits") || clone.contains("\"commits_imported\":1"),
        "clone output should describe native Git import: {clone}"
    );

    let status = heddle_without_git(&["status", "--output", "json"], &work).unwrap();
    let parsed: Value = serde_json::from_str(&status).unwrap();
    assert_eq!(parsed["repository_capability"], "git-overlay");
    assert_eq!(parsed["thread"], "main");

    std::fs::write(work.join("story.txt"), "written by heddle\n").unwrap();
    heddle_without_git(&["capture", "-m", "heddle change"], &work).unwrap();
    heddle_without_git(&["push"], &work).unwrap();

    let origin_repo = open_git(&origin).expect("open pushed origin");
    let new_tip = find_reference(&origin_repo, "refs/heads/main")
        .expect("main ref exists")
        .peel_to_id()
        .expect("peel main");
    assert_ne!(
        new_tip, original_tip,
        "heddle push should advance Git branch"
    );
}

#[test]
fn git_replacement_matrix_file_url_clone_and_import_without_git_on_path() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    let import_work = temp.path().join("import-work");
    seed_bare_git_repo(&origin);
    std::fs::create_dir(&import_work).expect("create import workdir");
    let origin_url = format!("file://{}", origin.display());

    let clone = heddle_without_git(
        &[
            "--output",
            "json",
            "clone",
            &origin_url,
            work.to_str().unwrap(),
        ],
        temp.path(),
    )
    .unwrap_or_else(|err| panic!("file:// clone should not require git helpers: {err}"));
    assert!(
        clone.contains("Imported 1 Git commits") || clone.contains("\"commits_imported\":1"),
        "file:// clone output should describe native Git import: {clone}"
    );
    configure_repo_local_git_identity(&work);
    let cloned_status = assert_clean_json_without_git(&["--output", "json", "status"], &work);
    assert_eq!(cloned_status["repository_capability"], "git-overlay");
    assert_eq!(cloned_status["thread"], "main");

    assert_clean_json_without_git(&["--output", "json", "init"], &import_work);
    let import = assert_clean_json_without_git(
        &[
            "--output",
            "json",
            "import",
            "git",
            "--path",
            &origin_url,
            "--ref",
            "main",
        ],
        &import_work,
    );
    assert!(
        import["commits_imported"].as_u64().unwrap_or(0) >= 1,
        "file:// Git import should copy locally without Git helpers: {import}"
    );
    let verify = assert_clean_json_without_git(&["--output", "json", "verify"], &import_work);
    assert_eq!(
        verify["verified"], true,
        "file:// import should leave verification clean without git on PATH: {verify}"
    );
}

#[test]
fn git_replacement_matrix_git_import_export_sync_reconcile_without_git_on_path() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    seed_bare_git_repo(&origin);

    heddle_without_git(
        &[
            "clone",
            origin.to_str().expect("origin path should be utf8"),
            work.to_str().expect("work path should be utf8"),
        ],
        temp.path(),
    )
    .unwrap();
    configure_repo_local_git_identity(&work);

    let origin_arg = origin.to_str().expect("origin path should be utf8");
    let import = assert_clean_json_without_git(
        &[
            "--output", "json", "import", "git", "--path", origin_arg, "--ref", "main",
        ],
        &work,
    );
    assert!(
        import["commits_imported"].as_u64().unwrap_or(0) >= 1,
        "explicit Git import should walk Git commits natively: {import}"
    );
    assert_eq!(import["output_kind"], "import_git");
    assert_eq!(
        import["verification"]["verified"], true,
        "Git import should embed post-operation verification: {import}"
    );

    let export_path = temp.path().join("exported.git");
    let export = assert_clean_json_without_git(
        &[
            "--output",
            "json",
            "export",
            "git",
            "--destination",
            export_path.to_str().expect("export path should be utf8"),
        ],
        &work,
    );
    assert!(
        export["states_exported"].as_u64().unwrap_or(0) >= 1
            || export["threads_synced"].as_u64().unwrap_or(0) >= 1,
        "explicit Git projection export should write Git-format refs natively: {export}"
    );
    let exported = open_git(&export_path).expect("open exported git repo");
    find_reference(&exported, "refs/heads/main").expect("export should write main branch");

    let sync = assert_clean_json_without_git(
        &["--output", "json", "sync", "git", "--path", origin_arg],
        &work,
    );
    assert_eq!(sync["output_kind"], "sync_git");

    let reconcile = assert_clean_json_without_git(
        &[
            "--output",
            "json",
            "fsck",
            "--repair",
            "git",
            "--ref",
            "main",
            "--preview",
        ],
        &work,
    );
    assert_eq!(reconcile["valid"], true);
    assert_eq!(reconcile["repair_target"], "git");
    assert_eq!(reconcile["repaired"], false);
    assert_eq!(
        reconcile["repairs"].as_array().map(Vec::len),
        Some(2),
        "fsck git repair preview should report both local repair choices: {reconcile}"
    );

    let verify = assert_clean_json_without_git(&["--output", "json", "verify"], &work);
    assert_eq!(
        verify["verified"], true,
        "explicit Git import operations should leave verification clean without git on PATH: {verify}"
    );
}

#[test]
fn git_replacement_matrix_commit_undo_rewinds_checkpoint_without_git_on_path() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    seed_bare_git_repo(&origin);

    heddle_without_git(
        &[
            "clone",
            origin.to_str().expect("origin path should be utf8"),
            work.to_str().expect("work path should be utf8"),
        ],
        temp.path(),
    )
    .unwrap();
    configure_repo_local_git_identity(&work);

    let base = git_head_oid(&work);
    std::fs::write(work.join("story.txt"), "undo without git\n").unwrap();
    let commit = assert_clean_json_without_git(
        &["--output", "json", "commit", "-m", "undo without git"],
        &work,
    );
    assert_eq!(commit["output_kind"], "commit");
    let after = git_head_oid(&work);
    assert_ne!(after, base, "commit should advance the checkout Git ref");

    let undo_list = assert_clean_json_without_git(
        &["--output", "json", "undo", "--list", "--depth", "1"],
        &work,
    );
    let operations = undo_list["batches"][0]["operations"].as_array().unwrap();
    assert!(
        operations.iter().any(|operation| operation["description"]
            .as_str()
            .is_some_and(|description| description.starts_with("git checkpoint "))),
        "undo list should expose the Git checkpoint inside the logical commit batch: {undo_list}"
    );

    let undo = assert_clean_json_without_git(&["--output", "json", "undo"], &work);
    assert_eq!(undo["action"], "undo");
    assert_eq!(
        git_head_oid(&work),
        base,
        "undo should rewind the visible Git checkout without invoking git"
    );

    let mirror = open_git(work.join(".heddle/git")).expect("open legacy Bridge Mirror");
    let mirror_tip = find_reference(&mirror, "refs/heads/main")
        .expect("mirror main exists")
        .peel_to_id()
        .expect("peel mirror main")
        .to_string();
    assert_eq!(
        mirror_tip, base,
        "undo should rewind the legacy Bridge Mirror branch without invoking git"
    );

    let status = assert_clean_json_without_git(&["--output", "json", "status"], &work);
    assert_eq!(status["verification"]["status"], "clean");
    assert!(
        status["changes"]["modified"].as_array().unwrap().is_empty()
            && status["changes"]["added"].as_array().unwrap().is_empty()
            && status["changes"]["deleted"].as_array().unwrap().is_empty(),
        "undo after commit should leave the worktree clean: {status}"
    );
}

/// heddle#305 (git-overlay): `commit` then `undo` hard-resets the legacy Bridge Mirror
/// to the parent — no revert commit recorded as Git history — while preserving
/// the pre-undo state in heddle's thread history via the internal
/// `undo-recovery` handle (heddle#305 r2: a heddle-internal ref, not a user
/// marker), so the absorbed worktree edits are never silently discarded. The
/// durability lives in heddle's store, not in Git history.
#[test]
fn git_replacement_matrix_undo_preserves_recovery_marker_for_absorbed_edit() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    seed_bare_git_repo(&origin);

    heddle_without_git(
        &[
            "clone",
            origin.to_str().expect("origin path should be utf8"),
            work.to_str().expect("work path should be utf8"),
        ],
        temp.path(),
    )
    .unwrap();
    configure_repo_local_git_identity(&work);

    let base = git_head_oid(&work);

    // An edit that lived only in the worktree, then absorbed by `commit`.
    std::fs::write(work.join("story.txt"), "FRICTION ONE\nFRICTION TWO\n").unwrap();
    let commit =
        assert_clean_json_without_git(&["--output", "json", "commit", "-m", "friction"], &work);
    assert_eq!(commit["output_kind"], "commit");
    let friction_state = commit["change_id"]
        .as_str()
        .expect("commit emits the absorbed heddle change-id")
        .to_string();
    let friction_commit = git_head_oid(&work);
    assert_ne!(
        friction_commit, base,
        "commit advances the checkout Git ref"
    );

    let undo = assert_clean_json_without_git(&["--output", "json", "undo"], &work);
    assert_eq!(undo["action"], "undo");

    // legacy Bridge Mirror is hard-reset to the parent — not a revert commit on top.
    assert_eq!(
        git_head_oid(&work),
        base,
        "undo must hard-reset the visible Git checkout to the parent"
    );
    let log = String::from_utf8(
        std::process::Command::new("git")
            .args(["log", "--oneline"])
            .current_dir(&work)
            .output()
            .expect("git log")
            .stdout,
    )
    .unwrap();
    assert!(
        !log.contains("friction"),
        "undo must not record itself as Git history (no revert/friction commit remains): {log}"
    );

    // The pre-undo state is preserved in heddle's thread history via the
    // internal recovery handle, even though Git was hard-reset. heddle#305 r2:
    // it must NOT leak into the user marker namespace.
    let markers =
        assert_clean_json_without_git(&["--output", "json", "thread", "marker", "list"], &work);
    assert!(
        markers["markers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["name"] != "undo-recovery"),
        "recovery bookkeeping must not appear as a user marker"
    );
    assert_eq!(undo["recovery_marker"], ".undo-recovery");
    assert_eq!(
        undo["recovery_state"], friction_state,
        "recovery handle must pin the pre-undo (friction) heddle state"
    );

    // And `redo` round-trips the absorbed content back into the worktree.
    let redo = assert_clean_json_without_git(&["--output", "json", "undo", "--redo"], &work);
    assert_eq!(redo["action"], "redo");
    assert_eq!(
        std::fs::read_to_string(work.join("story.txt")).unwrap(),
        "FRICTION ONE\nFRICTION TWO\n",
        "redo must restore the absorbed worktree edits"
    );
    assert_eq!(
        git_head_oid(&work),
        friction_commit,
        "redo must restore the Git checkpoint together with the heddle state"
    );
}

#[test]
fn git_replacement_matrix_commit_staged_index_without_git_on_path() {
    let temp = TempDir::new().unwrap();
    git_ok(&["init", "--initial-branch", "main"], temp.path());
    configure_repo_local_git_identity(temp.path());
    std::fs::write(temp.path().join("file.txt"), "base\n").unwrap();
    git_ok(&["add", "file.txt"], temp.path());
    git_ok(&["commit", "-m", "seed"], temp.path());

    assert_clean_json_without_git(&["--output", "json", "adopt", "--ref", "main"], temp.path());

    std::fs::write(temp.path().join("file.txt"), "staged\n").unwrap();
    git_ok(&["add", "file.txt"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "staged\nunstaged\n").unwrap();
    std::fs::write(temp.path().join("scratch.txt"), "left behind\n").unwrap();

    let status = assert_clean_json_without_git(&["--output", "json", "status"], temp.path());
    assert_eq!(status["git_index"]["commit_mode"], "staged_index");
    assert_eq!(status["git_index"]["has_staged_changes"], true);
    assert_eq!(
        status["git_index"]["staged_paths"],
        serde_json::json!(["file.txt"])
    );
    assert_eq!(
        status["git_index"]["unstaged_paths"],
        serde_json::json!(["file.txt"])
    );
    assert_eq!(
        status["git_index"]["untracked_paths"],
        serde_json::json!(["scratch.txt"])
    );
    assert_eq!(
        status["git_index"]["will_commit"],
        serde_json::json!(["file.txt"])
    );
    assert_eq!(
        status["git_index"]["preserved_after_commit"],
        serde_json::json!(["unstaged: file.txt", "untracked: scratch.txt"]),
        "status should predict exactly what plain `heddle commit` will leave behind: {status}"
    );

    let commit = assert_clean_json_without_git(
        &["--output", "json", "commit", "-m", "staged without git"],
        temp.path(),
    );
    assert_eq!(commit["output_kind"], "commit");
    assert_eq!(commit["git_index"]["commit_mode"], "staged_index");
    assert_eq!(
        commit["git_index"]["will_commit"],
        serde_json::json!(["file.txt"])
    );
    assert_eq!(
        commit["git_index"]["preserved_after_commit"],
        serde_json::json!(["unstaged: file.txt", "untracked: scratch.txt"]),
        "commit should repeat the same no-git index plan predicted by status: {commit}"
    );
    assert!(
        commit["summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("left 2 unstaged/untracked")),
        "staged commit should disclose preserved extra work: {commit}"
    );
    assert_eq!(
        git_stdout(&["show", "HEAD:file.txt"], temp.path()),
        "staged",
        "commit should write the staged index tree without invoking git from Heddle"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("file.txt")).unwrap(),
        "staged\nunstaged\n"
    );
    assert!(temp.path().join("scratch.txt").exists());
}

/// `git rm --cached path` stages a deletion without changing the
/// worktree, so `compare_worktree_cached_with_options` reports clean
/// even though the Git index has real intent. `heddle commit -m ...`
/// must consult the staged-index plan instead of short-circuiting on
/// the clean worktree and either reporting "nothing to commit" or
/// writing a generic checkpoint.
#[test]
fn git_replacement_matrix_commit_staged_removal_with_clean_worktree_without_git_on_path() {
    let temp = TempDir::new().unwrap();
    git_ok(&["init", "--initial-branch", "main"], temp.path());
    configure_repo_local_git_identity(temp.path());
    std::fs::write(temp.path().join("file.txt"), "keep\n").unwrap();
    git_ok(&["add", "file.txt"], temp.path());
    git_ok(&["commit", "-m", "seed"], temp.path());

    assert_clean_json_without_git(&["--output", "json", "adopt", "--ref", "main"], temp.path());

    git_ok(&["rm", "--cached", "file.txt"], temp.path());

    let status = assert_clean_json_without_git(&["--output", "json", "status"], temp.path());
    assert_eq!(status["git_index"]["commit_mode"], "staged_index");
    assert_eq!(
        status["git_index"]["staged_paths"],
        serde_json::json!(["file.txt"]),
        "staged removal must surface in the staged-index plan: {status}"
    );

    let commit = assert_clean_json_without_git(
        &["--output", "json", "commit", "-m", "drop staged"],
        temp.path(),
    );
    assert_eq!(
        commit["output_kind"], "commit",
        "clean-worktree+staged-removal must reach commit_staged_index, not the nothing-to-commit \
         or generic-checkpoint branch: {commit}"
    );
    assert_eq!(commit["git_index"]["commit_mode"], "staged_index");
    assert_eq!(
        commit["git_index"]["staged_paths"],
        serde_json::json!(["file.txt"])
    );
    assert_eq!(
        commit["git_index"]["will_commit"],
        serde_json::json!(["file.txt"])
    );

    assert_eq!(
        git_stdout(&["ls-tree", "HEAD", "file.txt"], temp.path()),
        "",
        "HEAD tree should no longer contain the removed path"
    );
    assert!(
        temp.path().join("file.txt").exists(),
        "git rm --cached must leave the worktree copy in place"
    );
}

#[test]
fn git_replacement_matrix_merge_git_commit_pushes_checkpoint_without_git_on_path() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    seed_bare_git_repo(&origin);

    heddle_without_git(
        &[
            "clone",
            origin.to_str().expect("origin path should be utf8"),
            work.to_str().expect("work path should be utf8"),
        ],
        temp.path(),
    )
    .unwrap();
    configure_repo_local_git_identity(&work);

    assert_clean_json_without_git(
        &["--output", "json", "thread", "create", "feature/no-git"],
        &work,
    );
    assert_clean_json_without_git(&["--output", "json", "switch", "feature/no-git"], &work);
    std::fs::write(work.join("feature.txt"), "merged without git\n").unwrap();
    assert_clean_json_without_git(
        &["--output", "json", "commit", "-m", "feature without git"],
        &work,
    );
    assert_clean_json_without_git(&["--output", "json", "switch", "main"], &work);

    let merge = assert_clean_json_without_git(
        &[
            "--output",
            "json",
            "merge",
            "feature/no-git",
            "-m",
            "merge without git",
            "--git-commit",
        ],
        &work,
    );
    assert_eq!(merge["status"], "completed");
    let merge_sha = merge["git_commit"]["sha"]
        .as_str()
        .expect("merge should report a Git checkpoint")
        .to_string();
    assert_eq!(git_head_oid(&work), merge_sha);

    let git_repo = open_git(&work).expect("open checkout git repo");
    let head = git_repo
        .find_commit(
            merge_sha
                .parse::<ObjectId>()
                .expect("merge sha should parse"),
        )
        .expect("merge checkpoint should exist");
    let message = head.message_raw_sloppy().to_string();
    assert!(
        message.starts_with("merge without git\n"),
        "checkpoint should preserve the user merge message: {message}"
    );
    assert!(
        head.tree()
            .expect("checkpoint tree")
            .lookup_entry_by_path("feature.txt")
            .expect("tree lookup")
            .is_some(),
        "checkpoint tree should come from the landed Heddle merge state"
    );

    heddle_without_git(&["push"], &work).unwrap();
    let origin_repo = open_git(&origin).expect("open origin");
    let origin_tip = find_reference(&origin_repo, "refs/heads/main")
        .expect("origin main exists")
        .peel_to_id()
        .expect("peel origin main")
        .to_string();
    assert_eq!(
        origin_tip, merge_sha,
        "push should send the native merge checkpoint instead of synthesizing a replacement commit"
    );
    assert_eq!(
        git_head_oid(&work),
        merge_sha,
        "push must not rewrite the local checkpoint commit"
    );
}

#[test]
fn git_replacement_matrix_branch_like_thread_refresh_without_git_on_path() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    seed_bare_git_repo(&origin);

    heddle_without_git(
        &[
            "clone",
            origin.to_str().expect("origin path should be utf8"),
            work.to_str().expect("work path should be utf8"),
        ],
        temp.path(),
    )
    .unwrap();
    configure_repo_local_git_identity(&work);

    assert_clean_json_without_git(
        &["--output", "json", "thread", "create", "feature/refresh"],
        &work,
    );
    assert_clean_json_without_git(&["--output", "json", "switch", "feature/refresh"], &work);
    std::fs::write(work.join("feature.txt"), "feature refresh\n").unwrap();
    assert_clean_json_without_git(
        &["--output", "json", "commit", "-m", "feature refresh"],
        &work,
    );

    assert_clean_json_without_git(&["--output", "json", "switch", "main"], &work);
    std::fs::write(work.join("main.txt"), "main refresh\n").unwrap();
    assert_clean_json_without_git(&["--output", "json", "commit", "-m", "main refresh"], &work);

    let blocked = heddle_output_without_git(
        &["--output", "json", "thread", "refresh", "feature/refresh"],
        &work,
    );
    let blocked_stdout = str::from_utf8(&blocked.stdout).unwrap_or("");
    let blocked_stderr = str::from_utf8(&blocked.stderr).unwrap_or("");
    assert!(
        !blocked.status.success(),
        "refreshing a branch-like thread from another checkout must ask for a switch first; stdout={blocked_stdout} stderr={blocked_stderr}"
    );
    assert!(
        !blocked_stderr.contains("No such file")
            && !blocked_stderr.contains("os error 2")
            && !blocked_stderr.contains("heddle init"),
        "refresh refusal should be typed recovery advice, not raw empty-path IO/init advice: {blocked_stderr}"
    );
    let envelope: Value =
        serde_json::from_str(blocked_stderr).expect("refresh refusal should emit JSON advice");
    assert_eq!(envelope["kind"], "thread_refresh_requires_checkout");
    assert_eq!(envelope["primary_command"], "heddle switch feature/refresh");
    assert_json_recovery_advice_fields(&envelope, "branch-like thread refresh refusal");
    assert_eq!(
        envelope["primary_command_template"]["argv_template"],
        heddle_argv_json(["switch", "feature/refresh"])
    );

    assert_clean_json_without_git(&["--output", "json", "switch", "feature/refresh"], &work);
    let refreshed = assert_clean_json_without_git(
        &["--output", "json", "thread", "refresh", "feature/refresh"],
        &work,
    );
    assert_eq!(refreshed["thread"]["freshness"], "current", "{refreshed}");
    assert_eq!(
        refreshed["thread"]["integration_policy_result"]["reason"],
        "thread refreshed cleanly onto target",
        "{refreshed}"
    );
    let verify = assert_verify_failed_json_without_git(&["--output", "json", "verify"], &work);
    assert_eq!(verify["status"], "needs_checkpoint", "{verify}");
    assert_eq!(
        verify["recommended_action"], "heddle checkpoint -m \"...\"",
        "{verify}"
    );
}

#[test]
fn git_replacement_matrix_remote_list_surfaces_git_overlay_origin_without_git_on_path() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    seed_bare_git_repo(&origin);

    heddle_without_git(
        &[
            "clone",
            origin.to_str().expect("origin path should be utf8"),
            work.to_str().expect("work path should be utf8"),
        ],
        temp.path(),
    )
    .unwrap();

    let output = heddle_without_git(&["remote", "list"], &work).unwrap();
    assert!(
        output.contains("origin") && output.contains(origin.to_str().unwrap()),
        "remote list should surface Git-overlay origin without a separate Heddle remote: {output}"
    );
    assert!(
        !output.contains("No remotes configured"),
        "Git-overlay remote config should not look empty: {output}"
    );
}

#[test]
fn git_replacement_matrix_remote_list_surfaces_all_git_overlay_remotes() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let upstream = temp.path().join("upstream.git");
    let work = temp.path().join("work");
    seed_bare_git_repo(&origin);
    seed_bare_git_repo(&upstream);

    heddle_without_git(
        &[
            "clone",
            origin.to_str().expect("origin path should be utf8"),
            work.to_str().expect("work path should be utf8"),
        ],
        temp.path(),
    )
    .unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(work.join(".git").join("config"))
        .unwrap()
        .write_all(
            format!(
                "\n[remote \"upstream\"]\n\turl = {}\n\tfetch = +refs/heads/*:refs/remotes/upstream/*\n",
                upstream.display()
            )
            .as_bytes(),
        )
        .unwrap();

    let output = heddle_without_git(&["remote", "list"], &work).unwrap();
    assert!(
        output.contains("origin") && output.contains(origin.to_str().unwrap()),
        "remote list should include origin: {output}"
    );
    assert!(
        output.contains("upstream") && output.contains(upstream.to_str().unwrap()),
        "remote list should include non-origin Git remotes: {output}"
    );
}

#[test]
fn git_replacement_matrix_checkpoint_writes_through_to_git_branch_and_index_without_git_on_path() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    let original_tip = seed_bare_git_repo(&origin);

    heddle_without_git(
        &[
            "clone",
            origin.to_str().expect("origin path should be utf8"),
            work.to_str().expect("work path should be utf8"),
        ],
        temp.path(),
    )
    .unwrap();
    configure_repo_local_git_identity(&work);
    std::fs::write(work.join("story.txt"), "captured by heddle\n").unwrap();
    heddle_without_git(&["capture", "-m", "write through"], &work).unwrap();
    heddle_without_git(&["checkpoint", "-m", "commit captured work"], &work).unwrap();

    let git_repo = open_git(&work).expect("open checkout git repo");
    let new_tip = find_reference(&git_repo, "refs/heads/main")
        .expect("main ref exists")
        .peel_to_id()
        .expect("peel main");
    assert_ne!(
        new_tip, original_tip,
        "checkpoint should advance the real Git branch ref"
    );
    assert!(
        work.join(".git").join("index").exists(),
        "checkpoint should rebuild the real Git index"
    );
    let tree = git_repo
        .find_commit(new_tip)
        .expect("tip should be a commit")
        .tree()
        .expect("tip should have a tree");
    assert!(
        tree.lookup_entry_by_path("story.txt")
            .expect("tree lookup")
            .is_some(),
        "write-through commit should contain captured file"
    );

    let status = heddle_without_git(&["status", "--output", "json"], &work).unwrap();
    let parsed: Value = serde_json::from_str(&status).expect("status should parse");
    assert_eq!(parsed["git_checkpoint"]["git_commit"], new_tip.to_string());
    assert_ne!(
        parsed["thread_health"], "blocked",
        "clean checkpointed work should not remain blocked: {status}"
    );
    assert_ne!(
        parsed["recommended_action"], "heddle thread promote main",
        "promotion can stay visible, but should not be the primary next action after checkpoint: {status}"
    );
}

#[test]
fn git_replacement_matrix_fsck_git_projection_validates_mapping_notes_and_checkout_without_git_on_path() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    seed_bare_git_repo(&origin);

    heddle_without_git(
        &[
            "clone",
            origin.to_str().expect("origin path should be utf8"),
            work.to_str().expect("work path should be utf8"),
        ],
        temp.path(),
    )
    .unwrap();
    configure_repo_local_git_identity(&work);
    std::fs::write(work.join("story.txt"), "git projection fsck\n").unwrap();
    heddle_without_git(&["capture", "-m", "git projection fsck"], &work).unwrap();
    heddle_without_git(&["checkpoint", "-m", "git projection fsck checkpoint"], &work).unwrap();

    let fsck = heddle_without_git(&["fsck", "--git", "--output", "json"], &work).unwrap();
    let parsed: Value = serde_json::from_str(&fsck).expect("fsck output should parse");
    assert_eq!(parsed["valid"], true, "Git projection fsck should pass: {fsck}");
    assert_eq!(parsed["git_projection_checked"], true);
}

#[test]
fn git_replacement_matrix_log_reflog_reads_checkout_logs_without_git_on_path() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    let seed = seed_bare_git_repo(&origin);

    heddle_without_git(
        &[
            "clone",
            origin.to_str().expect("origin path should be utf8"),
            work.to_str().expect("work path should be utf8"),
        ],
        temp.path(),
    )
    .unwrap();

    let logs = work.join(".git").join("logs").join("refs").join("heads");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(
        logs.join("main"),
        format!(
            "{zero} {seed} Heddle Test <heddle@test> 1770000000 +0000\tcheckpoint: seed\n",
            zero = "0".repeat(40),
            seed = seed
        ),
    )
    .unwrap();

    let output = heddle_without_git(&["log", "--reflog", "--output", "json"], &work).unwrap();
    let parsed: Value = serde_json::from_str(&output).expect("reflog JSON should parse");
    let entries = parsed["entries"].as_array().unwrap();
    assert!(
        entries.iter().any(|entry| {
            entry["source"] == "checkout"
                && entry["reference"] == "refs/heads/main"
                && entry["message"] == "checkpoint: seed"
        }),
        "reflog should include the branch checkout log entry: {output}"
    );
}

#[test]
fn git_replacement_matrix_checkpoint_reports_locked_index_without_git_on_path() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    seed_bare_git_repo(&origin);

    heddle_without_git(
        &[
            "clone",
            origin.to_str().expect("origin path should be utf8"),
            work.to_str().expect("work path should be utf8"),
        ],
        temp.path(),
    )
    .unwrap();
    configure_repo_local_git_identity(&work);
    std::fs::write(work.join("story.txt"), "locked index\n").unwrap();
    heddle_without_git(&["capture", "-m", "locked index"], &work).unwrap();
    std::fs::write(
        work.join(".git").join("index.lock"),
        b"held by another writer",
    )
    .unwrap();

    let err = heddle_without_git(&["checkpoint", "-m", "locked index"], &work)
        .expect_err("checkpoint should reject a locked Git index");
    assert!(
        err.contains("Git index is already locked"),
        "checkpoint should name the precise write-through skip reason: {err}"
    );
}

#[test]
fn git_replacement_matrix_pull_adopts_remote_branch_without_git_on_path() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    let original_tip = seed_bare_git_repo(&origin);

    heddle_without_git(
        &[
            "clone",
            origin.to_str().expect("origin path should be utf8"),
            work.to_str().expect("work path should be utf8"),
        ],
        temp.path(),
    )
    .unwrap();

    let origin_repo = open_git(&origin).expect("open origin");
    let advanced_tip = git_commit_with_tree(
        &origin_repo,
        Some("refs/heads/main"),
        git_empty_tree_oid(&origin_repo),
        "remote advance",
        &[original_tip],
    );
    assert_ne!(advanced_tip, original_tip);

    let pull = heddle_without_git(
        &[
            "pull",
            origin.to_str().expect("origin path should be utf8"),
            "--output",
            "text",
        ],
        &work,
    )
    .unwrap();
    assert!(
        pull.contains("pulled from")
            && pull.contains("Branch:")
            && pull.contains("Git:")
            && pull.contains("Imported:")
            && pull.contains("Changed paths:")
            && pull.contains("Workspace: verified"),
        "pull text should explain remote movement without requiring git on PATH: {pull}"
    );

    let mirror = open_git(work.join(".heddle/git")).expect("open legacy Bridge Mirror");
    let mirror_tip = find_reference(&mirror, "refs/heads/main")
        .expect("mirror main exists")
        .peel_to_id()
        .expect("peel mirror main");
    assert_eq!(
        mirror_tip, advanced_tip,
        "heddle pull should advance the native legacy Bridge Mirror without using git on PATH"
    );
}

#[test]
fn git_replacement_matrix_fetch_does_not_dirty_checkout_and_pull_materializes_without_git_on_path()
{
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    let origin_repo = SleyRepository::init_bare(&origin).expect("init bare git repo");
    let base_tree = git_tree_with_file(&origin_repo, "shared.txt", b"base\n");
    let original_tip = git_commit_with_tree(
        &origin_repo,
        Some("refs/heads/main"),
        base_tree,
        "seed",
        &[],
    );
    git_set_reference(&origin_repo, "HEAD", original_tip);

    heddle_without_git(
        &[
            "clone",
            origin.to_str().expect("origin path should be utf8"),
            work.to_str().expect("work path should be utf8"),
        ],
        temp.path(),
    )
    .unwrap();

    let advanced_tree = git_tree_with_file(&origin_repo, "shared.txt", b"base\nupstream\n");
    let advanced_tip = git_commit_with_tree(
        &origin_repo,
        Some("refs/heads/main"),
        advanced_tree,
        "upstream",
        &[original_tip],
    );
    assert_ne!(advanced_tip, original_tip);

    heddle_without_git(
        &[
            "fetch",
            origin.to_str().expect("origin path should be utf8"),
        ],
        &work,
    )
    .unwrap();
    let fetched_status = heddle_without_git(&["status", "--output", "json"], &work).unwrap();
    let fetched_status: Value = serde_json::from_str(&fetched_status).unwrap();
    assert_eq!(
        fetched_status["changes"]["modified"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        std::fs::read_to_string(work.join("shared.txt")).unwrap(),
        "base\n"
    );

    let pull_json = assert_clean_json_without_git(
        &[
            "--output",
            "json",
            "pull",
            origin.to_str().expect("origin path should be utf8"),
        ],
        &work,
    );
    assert_eq!(pull_json["branch"], "main");
    assert_eq!(pull_json["old_git_head"], original_tip.to_string());
    assert_eq!(pull_json["new_git_head"], advanced_tip.to_string());
    assert_eq!(pull_json["changed_path_count"], 1);
    assert_eq!(
        pull_json["changed_paths"],
        serde_json::json!(["shared.txt"])
    );
    assert_eq!(pull_json["verification"]["verified"], true);
    let pulled_status = heddle_without_git(&["status", "--output", "json"], &work).unwrap();
    let pulled_status: Value = serde_json::from_str(&pulled_status).unwrap();
    assert_eq!(
        pulled_status["changes"]["modified"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        std::fs::read_to_string(work.join("shared.txt")).unwrap(),
        "base\nupstream\n"
    );
}

#[test]
fn git_replacement_matrix_fetch_discovers_new_remote_branch_without_git_on_path() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    let origin_repo = SleyRepository::init_bare(&origin).expect("init bare git repo");
    let base_tree = git_tree_with_file(&origin_repo, "shared.txt", b"base\n");
    let original_tip = git_commit_with_tree(
        &origin_repo,
        Some("refs/heads/main"),
        base_tree,
        "seed",
        &[],
    );
    git_set_reference(&origin_repo, "HEAD", original_tip);

    heddle_without_git(
        &[
            "clone",
            origin.to_str().expect("origin path should be utf8"),
            work.to_str().expect("work path should be utf8"),
        ],
        temp.path(),
    )
    .unwrap();

    let topic_tree = git_tree_with_file(&origin_repo, "topic.txt", b"remote topic\n");
    let topic_tip = git_commit_with_tree(
        &origin_repo,
        Some("refs/heads/topic-remote"),
        topic_tree,
        "topic remote",
        &[original_tip],
    );

    heddle_without_git(
        &[
            "fetch",
            origin.to_str().expect("origin path should be utf8"),
        ],
        &work,
    )
    .unwrap();

    let checkout = open_git(&work).expect("open checkout git repo");
    let checkout_topic = find_reference(&checkout, "refs/remotes/origin/topic-remote")
        .expect("fetch should discover checkout remote-tracking branch")
        .peel_to_id()
        .expect("peel checkout remote-tracking branch");
    assert_eq!(checkout_topic, topic_tip);

    let mirror = open_git(work.join(".heddle/git")).expect("open legacy Bridge Mirror");
    let mirror_topic = find_reference(&mirror, "refs/remotes/origin/topic-remote")
        .expect("fetch should mirror remote-tracking branch")
        .peel_to_id()
        .expect("peel mirror remote-tracking branch");
    assert_eq!(mirror_topic, topic_tip);

    let import = assert_clean_json_without_git(
        &[
            "--output",
            "json",
            "import",
            "git",
            "--ref",
            "origin/topic-remote",
        ],
        &work,
    );
    assert_eq!(import["branches_synced"], 1, "{import}");
    let threads = assert_clean_json_without_git(&["--output", "json", "thread", "list"], &work);
    assert!(
        threads["threads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|thread| thread["name"] == "origin/topic-remote"),
        "imported remote branch should be visible as a Heddle thread: {threads}"
    );
}

#[test]
fn git_replacement_matrix_https_push_uses_native_transport_without_git_on_path() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    seed_bare_git_repo(&origin);

    heddle_without_git(
        &[
            "clone",
            origin.to_str().expect("origin path should be utf8"),
            work.to_str().expect("work path should be utf8"),
        ],
        temp.path(),
    )
    .unwrap();
    configure_repo_local_git_identity(&work);
    std::fs::write(work.join("story.txt"), "https push attempt\n").unwrap();
    heddle_without_git(&["capture", "-m", "attempt https push"], &work).unwrap();

    let listener = match std::net::TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("skipping HTTPS native transport test: loopback bind denied: {err}");
            return;
        }
        Err(err) => panic!("reserve local port: {err}"),
    };
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    let err = heddle_without_git(
        &["push", &format!("https://127.0.0.1:{port}/repo.git")],
        &work,
    )
    .expect_err("closed HTTPS endpoint should fail after choosing native transport");

    let lowercase_error = err.to_ascii_lowercase();
    assert!(
        lowercase_error.contains("failed to connect")
            || lowercase_error.contains("receive-pack handshake failed")
            || lowercase_error.contains("connection"),
        "HTTPS push should fail as a native transport connection error: {err}"
    );
    assert!(
        !err.contains("not implemented yet") && !err.contains("only local path"),
        "HTTPS push must not regress to the old local-only placeholder: {err}"
    );
}
