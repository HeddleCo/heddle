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
    serde_json::from_str(stdout)
        .unwrap_or_else(|err| panic!("{args:?} should emit parseable JSON: {err}: {stdout}"))
}

fn seed_bare_git_repo(path: &std::path::Path) -> gix::hash::ObjectId {
    let repo = gix::init_bare(path).expect("init bare git repo");
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

fn git_tree_with_file(repo: &gix::Repository, path: &str, content: &[u8]) -> gix::hash::ObjectId {
    let blob = repo.write_blob(content).expect("write git blob").detach();
    let empty = git_empty_tree_oid(repo);
    let mut editor = repo.edit_tree(empty).expect("edit git tree");
    editor
        .upsert(path, gix::object::tree::EntryKind::Blob, blob)
        .expect("add file to git tree");
    editor.write().expect("write git tree").detach()
}

#[test]
fn git_replacement_matrix_fresh_git_read_commands_without_git_on_path() {
    let temp = TempDir::new().unwrap();
    gix::init(temp.path()).expect("init git worktree");
    std::fs::write(temp.path().join("pending.txt"), "pending\n").unwrap();

    let status = heddle_without_git(&["status", "--json"], temp.path()).unwrap();
    let parsed: Value = serde_json::from_str(&status).expect("status should parse");
    assert_eq!(parsed["repository_capability"], "git-overlay");
    assert_eq!(parsed["thread"], "main");
    assert!(
        parsed["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "pending.txt"),
        "fresh git status should report pending work without git on PATH: {status}"
    );

    for args in [
        &["diagnose", "--json"][..],
        &["thread", "list", "--json"],
        &["workspace", "show", "--json"],
    ] {
        let stdout = heddle_without_git(args, temp.path())
            .unwrap_or_else(|err| panic!("{args:?} should not require git on PATH: {err}"));
        let parsed: Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|err| panic!("{args:?} should emit JSON: {err}: {stdout}"));
        assert_eq!(
            parsed["repository_capability"], "git-overlay",
            "{args:?} should preserve overlay capability: {stdout}"
        );
    }

    let ready = heddle_without_git(&["ready", "--json"], temp.path())
        .unwrap_or_else(|err| panic!("ready --json should not require git on PATH: {err}"));
    let ready: Value = serde_json::from_str(&ready)
        .unwrap_or_else(|err| panic!("ready --json should emit JSON: {err}: {ready}"));
    assert!(
        ready["status"].is_string(),
        "ready should report status: {ready}"
    );
}

#[test]
fn git_replacement_matrix_everyday_save_read_machine_streams_without_git_on_path() {
    let temp = TempDir::new().unwrap();
    gix::init(temp.path()).expect("init git worktree");

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
        &["--output", "json", "diagnose"],
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
            "clone",
            origin.to_str().expect("origin path should be utf8"),
            work.to_str().expect("work path should be utf8"),
        ],
        temp.path(),
    )
    .unwrap();
    assert!(
        clone.contains("Imported 1 Git commits") || clone.contains("\"commits_imported\":1"),
        "clone output should describe native Git import: {clone}"
    );

    let status = heddle_without_git(&["status", "--json"], &work).unwrap();
    let parsed: Value = serde_json::from_str(&status).unwrap();
    assert_eq!(parsed["repository_capability"], "git-overlay");
    assert_eq!(parsed["thread"], "main");

    std::fs::write(work.join("story.txt"), "written by heddle\n").unwrap();
    heddle_without_git(&["capture", "-m", "heddle change"], &work).unwrap();
    heddle_without_git(
        &["push", origin.to_str().expect("origin path should be utf8")],
        &work,
    )
    .unwrap();

    let origin_repo = gix::open(&origin).expect("open pushed origin");
    let new_tip = origin_repo
        .find_reference("refs/heads/main")
        .expect("main ref exists")
        .peel_to_id()
        .expect("peel main")
        .detach();
    assert_ne!(
        new_tip, original_tip,
        "heddle push should advance Git branch"
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
    std::fs::write(work.join("story.txt"), "captured by heddle\n").unwrap();
    heddle_without_git(&["capture", "-m", "write through"], &work).unwrap();
    heddle_without_git(&["checkpoint", "-m", "commit captured work"], &work).unwrap();

    let git_repo = gix::open(&work).expect("open checkout git repo");
    let new_tip = git_repo
        .find_reference("refs/heads/main")
        .expect("main ref exists")
        .peel_to_id()
        .expect("peel main")
        .detach();
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

    let status = heddle_without_git(&["status", "--json"], &work).unwrap();
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
fn git_replacement_matrix_fsck_bridge_validates_mapping_notes_and_checkout_without_git_on_path() {
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
    std::fs::write(work.join("story.txt"), "fsck bridge\n").unwrap();
    heddle_without_git(&["capture", "-m", "fsck bridge"], &work).unwrap();
    heddle_without_git(&["checkpoint", "-m", "fsck bridge checkpoint"], &work).unwrap();

    let fsck = heddle_without_git(&["fsck", "--bridge", "--json"], &work).unwrap();
    let parsed: Value = serde_json::from_str(&fsck).expect("fsck output should parse");
    assert_eq!(parsed["valid"], true, "bridge fsck should pass: {fsck}");
    assert_eq!(parsed["bridge_checked"], true);
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

    let output = heddle_without_git(&["log", "--reflog", "--json"], &work).unwrap();
    let parsed: Value = serde_json::from_str(&output).expect("reflog JSON should parse");
    assert_eq!(parsed["entries"].as_array().unwrap().len(), 1, "{output}");
    assert_eq!(parsed["entries"][0]["source"], "checkout");
    assert_eq!(parsed["entries"][0]["reference"], "refs/heads/main");
    assert_eq!(parsed["entries"][0]["message"], "checkpoint: seed");
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

    let origin_repo = gix::open(&origin).expect("open origin");
    let advanced_tip = git_commit_with_tree(
        &origin_repo,
        Some("refs/heads/main"),
        git_empty_tree_oid(&origin_repo),
        "remote advance",
        &[original_tip],
    );
    assert_ne!(advanced_tip, original_tip);

    heddle_without_git(
        &["pull", origin.to_str().expect("origin path should be utf8")],
        &work,
    )
    .unwrap();

    let mirror = gix::open(work.join(".heddle/git")).expect("open Heddle Git mirror");
    let mirror_tip = mirror
        .find_reference("refs/heads/main")
        .expect("mirror main exists")
        .peel_to_id()
        .expect("peel mirror main")
        .detach();
    assert_eq!(
        mirror_tip, advanced_tip,
        "heddle pull should advance the native Git mirror without using git on PATH"
    );
}

#[test]
fn git_replacement_matrix_fetch_does_not_dirty_checkout_and_pull_materializes_without_git_on_path()
{
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    let origin_repo = gix::init_bare(&origin).expect("init bare git repo");
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
    let fetched_status = heddle_without_git(&["status", "--json"], &work).unwrap();
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

    heddle_without_git(
        &["pull", origin.to_str().expect("origin path should be utf8")],
        &work,
    )
    .unwrap();
    let pulled_status = heddle_without_git(&["status", "--json"], &work).unwrap();
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
    std::fs::write(work.join("story.txt"), "https push attempt\n").unwrap();
    heddle_without_git(&["capture", "-m", "attempt https push"], &work).unwrap();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve local port");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    let err = heddle_without_git(
        &["push", &format!("https://127.0.0.1:{port}/repo.git")],
        &work,
    )
    .expect_err("closed HTTPS endpoint should fail after choosing native transport");

    assert!(
        err.contains("failed to connect")
            || err.contains("receive-pack handshake failed")
            || err.contains("connection"),
        "HTTPS push should fail as a native transport connection error: {err}"
    );
    assert!(
        !err.contains("not implemented yet") && !err.contains("only local path"),
        "HTTPS push must not regress to the old local-only placeholder: {err}"
    );
}
