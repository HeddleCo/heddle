// SPDX-License-Identifier: Apache-2.0
use super::*;

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

fn git_commit_all(path: &std::path::Path, message: &str) {
    git(&["add", "."], path);
    git(&["commit", "-m", message], path);
}

fn json(cwd: &std::path::Path, args: &[&str]) -> Value {
    serde_json::from_str(&heddle(args, Some(cwd)).unwrap())
        .unwrap_or_else(|err| panic!("expected JSON for {:?}: {}", args, err))
}

fn assert_git_overlay_basics(parsed: &Value) {
    assert_eq!(parsed["repository_capability"], "git-overlay");
    assert_eq!(parsed["storage_model"], "git+heddle-sidecar");
}

fn assert_trust_check_rows(parsed: &Value) {
    let checks = parsed["checks"]
        .as_array()
        .unwrap_or_else(|| panic!("trust output should expose checks: {parsed}"));
    for row in [
        "Git",
        "Heddle",
        "Mapping",
        "Worktree",
        "Remote",
        "Operation",
        "Machine contract",
        "Clone",
    ] {
        assert!(
            checks.iter().any(|check| check["name"] == row),
            "trust output should include `{row}` row: {parsed}"
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

#[test]
fn git_overlay_matrix_plain_git_no_commit_bootstrap_commands() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "trunk");

    std::fs::write(temp.path().join("pending.txt"), "pending").unwrap();

    let status = json(temp.path(), &["status", "--json"]);
    assert_eq!(status["repository_capability"], "plain-git");
    assert_eq!(status["heddle_initialized"], false);
    assert_eq!(status["git_branch"], "trunk");
    assert_eq!(status["git_overlay_health"]["status"], "needs_init");
    assert!(
        !temp.path().join(".heddle").exists(),
        "status in a plain Git repo must be probe-only"
    );

    heddle(&["init"], Some(temp.path())).unwrap();

    let diagnose = json(temp.path(), &["diagnose", "--json"]);
    assert_git_overlay_basics(&diagnose);
    assert_eq!(diagnose["thread"]["name"], "trunk");

    let thread_list = json(temp.path(), &["thread", "list", "--json"]);
    assert_eq!(thread_list["current"], "trunk");

    let workspace = json(temp.path(), &["workspace", "show", "--json"]);
    assert_eq!(workspace["current_thread"], "trunk");

    let show = json(temp.path(), &["show", "HEAD", "--json"]);
    assert_git_overlay_basics(&show);
    assert!(show["change_id"].as_str().is_some());

    let log = json(temp.path(), &["log", "--json"]);
    assert_git_overlay_basics(&log);
    assert!(
        !log["states"].as_array().unwrap().is_empty(),
        "log should bootstrap a visible state in plain Git no-commit repos: {log}"
    );
}

#[test]
fn git_overlay_matrix_trust_tracks_plain_init_import_clean_loop() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");

    let trust = json(temp.path(), &["trust", "--json"]);
    assert_eq!(trust["trusted"], false);
    assert_eq!(trust["status"], "needs_init");
    assert_eq!(trust["recommended_action"], "heddle init");
    assert_trust_check_rows(&trust);
    let status = json(temp.path(), &["status", "--json"]);
    assert_eq!(status["trust"]["trusted"], false);
    assert_eq!(status["trust"]["status"], "needs_init");
    assert_eq!(status["recommended_action"], "heddle init");
    assert_eq!(status["recovery_commands"][0], "heddle init");
    assert_trust_check_rows(&status["trust"]);
    assert!(
        !temp.path().join(".heddle").exists(),
        "trust in a plain Git repo must be observe-only"
    );

    let bridge = json(temp.path(), &["bridge", "git", "status", "--json"]);
    assert_eq!(bridge["git_overlay_health"]["status"], "needs_init");
    assert_eq!(bridge["trust"]["status"], "needs_init");
    assert_trust_check_rows(&bridge["trust"]);
    assert!(
        !temp.path().join(".heddle").exists(),
        "bridge git status in a plain Git repo must be observe-only"
    );

    heddle(&["init"], Some(temp.path())).unwrap();
    let trust = json(temp.path(), &["trust", "--json"]);
    assert_eq!(trust["trusted"], false);
    assert_eq!(trust["status"], "needs_import");
    assert_trust_check_rows(&trust);
    assert_eq!(
        trust["recommended_action"],
        "heddle bridge git import --ref main"
    );
    let status = json(temp.path(), &["status", "--json"]);
    assert_eq!(status["trust"]["trusted"], false);
    assert_eq!(status["trust"]["status"], "needs_import");
    assert_eq!(
        status["recommended_action"],
        "heddle bridge git import --ref main"
    );
    assert_eq!(
        status["recovery_commands"][0],
        "heddle bridge git import --ref main"
    );
    assert_trust_check_rows(&status["trust"]);
    let workspace = json(temp.path(), &["workspace", "show", "--json"]);
    assert_eq!(workspace["trust"]["trusted"], false);
    assert_eq!(workspace["trust"]["status"], "needs_import");
    assert_eq!(
        workspace["recommended_action"],
        "heddle bridge git import --ref main"
    );
    assert_trust_check_rows(&workspace["trust"]);
    let thread_list = json(temp.path(), &["thread", "list", "--json"]);
    assert_eq!(thread_list["trust"]["trusted"], false);
    assert_eq!(thread_list["trust"]["status"], "needs_import");
    assert_eq!(
        thread_list["recommended_action"],
        "heddle bridge git import --ref main"
    );
    assert_trust_check_rows(&thread_list["trust"]);
    let thread_show = json(temp.path(), &["thread", "show", "main", "--json"]);
    assert_eq!(thread_show["trust"]["trusted"], false);
    assert_eq!(thread_show["trust"]["status"], "needs_import");
    assert_eq!(
        thread_show["recommended_action"],
        "heddle bridge git import --ref main"
    );
    assert_trust_check_rows(&thread_show["trust"]);
    let diagnose = json(temp.path(), &["diagnose", "--json"]);
    assert_eq!(diagnose["trust"]["trusted"], false);
    assert_eq!(diagnose["trust"]["status"], "needs_import");
    assert_eq!(
        diagnose["trust"]["recommended_action"],
        "heddle bridge git import --ref main"
    );

    heddle(
        &["bridge", "git", "import", "--ref", "main"],
        Some(temp.path()),
    )
    .unwrap();
    let trust = json(temp.path(), &["trust", "--json"]);
    assert_eq!(trust["trusted"], true);
    assert_eq!(trust["status"], "clean");
    assert_trust_check_rows(&trust);
    let trust_text = heddle(&["trust", "--output", "text"], Some(temp.path())).unwrap();
    assert!(
        !trust_text.contains("not checked"),
        "clean trust text should render all proof rows as checked: {trust_text}"
    );
    let diagnose = json(temp.path(), &["diagnose", "--json"]);
    assert_eq!(diagnose["trust"]["trusted"], true);
    assert_eq!(diagnose["trust"]["status"], "clean");
    let status = json(temp.path(), &["status", "--json"]);
    assert_eq!(status["trust"]["trusted"], true);
    assert_eq!(status["trust"]["status"], "clean");
    assert_eq!(
        status["recovery_commands"]
            .as_array()
            .expect("status recovery commands should be an array")
            .len(),
        0
    );
    assert_trust_check_rows(&status["trust"]);
    let workspace = json(temp.path(), &["workspace", "show", "--json"]);
    assert_eq!(workspace["trust"]["trusted"], true);
    assert_eq!(workspace["trust"]["status"], "clean");
    assert_trust_check_rows(&workspace["trust"]);
    let thread_list = json(temp.path(), &["thread", "list", "--json"]);
    assert_eq!(thread_list["trust"]["trusted"], true);
    assert_eq!(thread_list["trust"]["status"], "clean");
    assert_eq!(thread_list["recommended_action"], "");
    assert_trust_check_rows(&thread_list["trust"]);
    let thread_show = json(temp.path(), &["thread", "show", "main", "--json"]);
    assert_eq!(thread_show["trust"]["trusted"], true);
    assert_eq!(thread_show["trust"]["status"], "clean");
    assert_trust_check_rows(&thread_show["trust"]);
    let ready = json(temp.path(), &["--json", "ready"]);
    assert_eq!(ready["trust"]["trusted"], true);
    assert_eq!(ready["trust"]["status"], "clean");
    assert_trust_check_rows(&ready["trust"]);
}

#[test]
fn git_overlay_matrix_ready_blocks_when_repository_trust_needs_import() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle(&["init"], Some(temp.path())).unwrap();

    let ready = json(temp.path(), &["--json", "ready"]);
    assert_eq!(ready["status"], "blocked");
    assert_eq!(ready["trust"]["trusted"], false);
    assert_eq!(ready["trust"]["status"], "needs_import");
    assert_eq!(
        ready["recommended_action"],
        "heddle bridge git import --ref main"
    );
    assert!(
        ready["message"]
            .as_str()
            .is_some_and(|message| message.contains("repository trust is blocked")),
        "ready should name the trust blocker instead of claiming ready: {ready}"
    );
    assert_trust_check_rows(&ready["trust"]);
}

#[test]
fn git_overlay_matrix_thread_and_workspace_plain_git_are_observe_only() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");

    let thread_list = json(temp.path(), &["thread", "list", "--json"]);
    assert_eq!(thread_list["repository_capability"], "plain-git");
    assert_eq!(thread_list["recommended_action"], "heddle init");
    assert!(
        !temp.path().join(".heddle").exists(),
        "thread list in a plain Git repo must be observe-only"
    );

    let thread_show = json(temp.path(), &["thread", "show", "main", "--json"]);
    assert_eq!(thread_show["repository_capability"], "plain-git");
    assert_eq!(thread_show["recommended_action"], "heddle init");
    assert!(
        !temp.path().join(".heddle").exists(),
        "thread show in a plain Git repo must be observe-only"
    );

    let workspace = json(temp.path(), &["workspace", "show", "--json"]);
    assert_eq!(workspace["repository_capability"], "plain-git");
    assert_eq!(workspace["trust"]["status"], "needs_init");
    assert_eq!(workspace["recommended_action"], "heddle init");
    assert_trust_check_rows(&workspace["trust"]);
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
        ("status", &["status", "--json"]),
        ("diagnose", &["diagnose", "--json"]),
        ("doctor", &["doctor", "--output", "json"]),
        ("bridge git status", &["bridge", "git", "status", "--json"]),
        ("trust", &["trust", "--json"]),
        ("thread list", &["thread", "list", "--json"]),
        ("thread show", &["thread", "show", "main", "--json"]),
        ("workspace show", &["workspace", "show", "--json"]),
        ("log", &["log", "--json"]),
        ("show", &["show", "HEAD", "--json"]),
        ("diff", &["diff", "--json"]),
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
fn git_overlay_matrix_reconcile_apply_imports_current_git_branch() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle(&["init"], Some(temp.path())).unwrap();

    let status = json(temp.path(), &["status", "--json"]);
    assert_eq!(status["git_overlay_health"]["status"], "needs_import");
    assert_eq!(
        status["recommended_action"],
        "heddle bridge git import --ref main"
    );

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

    let status = json(temp.path(), &["status", "--json"]);
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
        envelope["error"].as_str().is_some_and(|error| error
            .contains("no matching Heddle thread exists")
            && error.contains("Unsafe:")
            && error.contains("Would change:")
            && error.contains("Preserved:")
            && error.contains("Primary recovery:")),
        "reconcile refusal should include full typed advice: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle bridge git import --ref main")
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
            .is_some_and(|error| error.contains("nothing to commit")
                && error.contains("Unsafe:")
                && error.contains("Would change:")
                && error.contains("Preserved:")
                && error.contains("Primary recovery:")),
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
fn git_overlay_matrix_commit_noop_fails_closed_when_trust_blocked() {
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
        "trusted-blocked commit should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode trusted-blocked commit refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: serde_json::Value =
        serde_json::from_str(stderr).expect("trust-blocked commit should emit JSON envelope");
    assert_eq!(envelope["kind"], "commit_blocked_by_trust");
    assert!(
        envelope["error"].as_str().is_some_and(|error| {
            error.contains("repository trust is blocked")
                && error.contains("operation_in_progress")
                && error.contains("Unsafe:")
                && error.contains("Would change:")
                && error.contains("Preserved:")
                && error.contains("Primary recovery:")
        }),
        "trust-blocked no-op commit should refuse with full typed advice: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle continue")),
        "trust-blocked no-op commit should name the trust recovery command: {stderr}"
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
    let commit = json(temp.path(), &["--json", "commit", "-m", "change"]);
    assert!(commit["git_commit"].as_str().is_some());
    let after = git_stdout(temp.path(), &["rev-parse", "HEAD"]);
    assert_ne!(after, base);

    let undo_list = json(temp.path(), &["--json", "undo", "--list", "--depth", "1"]);
    let operations = undo_list["batches"][0]["operations"]
        .as_array()
        .expect("undo list should expose operations");
    assert_eq!(
        operations.len(),
        2,
        "compat commit should be one logical undo batch containing capture + Git checkpoint: {undo_list}"
    );
    assert!(
        operations.iter().any(|op| op["description"]
            .as_str()
            .is_some_and(|description| description.starts_with("snapshot "))),
        "commit undo batch should include the captured Heddle state: {undo_list}"
    );
    assert!(
        operations.iter().any(|op| op["description"]
            .as_str()
            .is_some_and(|description| description.starts_with("git checkpoint "))),
        "commit undo batch should include the Git checkpoint: {undo_list}"
    );

    let undo = json(temp.path(), &["--json", "undo"]);
    assert_eq!(undo["action"], "undo");
    assert_eq!(git_stdout(temp.path(), &["rev-parse", "HEAD"]), base);
    assert_eq!(git_stdout(temp.path(), &["status", "--short"]), "");
    let status = json(temp.path(), &["--json", "status"]);
    assert_eq!(status["git_overlay_health"]["status"], "clean");
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
    let commit = json(temp.path(), &["--json", "commit", "-m", "change"]);
    let expected_git = commit["git_commit"]
        .as_str()
        .expect("commit should report Git checkpoint")
        .to_string();
    let heddle_after_commit = json(temp.path(), &["--json", "status"])["current_state"]
        .as_str()
        .expect("status should report current Heddle state")
        .to_string();

    git(&["reset", "--soft", "HEAD~1"], temp.path());
    let current_git = git_stdout(temp.path(), &["rev-parse", "HEAD"]);
    assert_ne!(current_git, expected_git);

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
    assert_eq!(envelope["kind"], "git_head_mismatch");
    assert!(
        envelope["error"].as_str().is_some_and(|error| {
            error.contains(&current_git)
                && error.contains(&expected_git)
                && error.contains("dirty paths: tracked.txt")
                && error
                    .contains("heddle bridge git reconcile --prefer heddle --ref main --preview")
        }),
        "unsafe undo should name current/expected Git OIDs and reconcile preview: {stderr}"
    );

    assert_eq!(git_stdout(temp.path(), &["rev-parse", "HEAD"]), current_git);
    let status_after = json(temp.path(), &["--json", "status"]);
    assert_eq!(
        status_after["current_state"], heddle_after_commit,
        "unsafe Git undo must leave Heddle state untouched: {status_after}"
    );
}

#[test]
fn git_overlay_matrix_bridge_push_pull_report_trust_state() {
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
    json(temp.path(), &["--json", "commit", "-m", "change"]);
    let origin_arg = origin.path().to_str().expect("origin path should be utf8");

    let push = json(
        temp.path(),
        &["--json", "bridge", "git", "push", origin_arg],
    );
    assert_eq!(push["pushed"], true);
    assert_eq!(push["trust"]["trusted"], true);
    assert_eq!(push["trust"]["status"], "clean");
    assert_trust_check_rows(&push["trust"]);

    let pull = json(
        temp.path(),
        &["--json", "bridge", "git", "pull", origin_arg],
    );
    assert_eq!(pull["pulled"], true);
    assert_eq!(pull["trust"]["trusted"], true);
    assert_eq!(pull["trust"]["status"], "clean");
    assert_trust_check_rows(&pull["trust"]);
}

#[test]
fn git_overlay_matrix_subdirectory_dirty_commands() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(&["status", "--json"], Some(temp.path())).unwrap();

    let nested = temp.path().join("src/deep/nested");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "tracked modified").unwrap();
    std::fs::write(temp.path().join("new.txt"), "new").unwrap();

    let status = json(&nested, &["status", "--json"]);
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

    let diagnose = json(&nested, &["diagnose", "--json"]);
    assert_eq!(diagnose["changes"]["total"], 2);

    let show = json(&nested, &["show", "HEAD", "--json"]);
    assert!(show["change_id"].as_str().is_some());

    let log = json(&nested, &["log", "--json"]);
    assert!(
        !log["states"].as_array().unwrap().is_empty(),
        "log should resolve from nested repo paths: {log}"
    );

    let diff = json(&nested, &["diff", "HEAD"]);
    assert!(
        diff["changes"].as_array().is_some(),
        "diff should remain well-formed after nested-path bootstrap/show/log sequencing: {diff}"
    );

    let thread_list = json(&nested, &["thread", "list", "--json"]);
    assert_eq!(thread_list["current"], "feature/drop-in");

    let workspace = json(&nested, &["workspace", "show", "--json"]);
    assert_eq!(workspace["current_thread"], "feature/drop-in");
}

#[test]
fn git_overlay_matrix_manual_git_commit_after_bootstrap_commands() {
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

    std::fs::write(temp.path().join("tracked.txt"), "tracked committed via git").unwrap();
    git(&["add", "tracked.txt"], temp.path());
    git(&["commit", "-m", "manual git commit"], temp.path());

    let status = json(temp.path(), &["status", "--json"]);
    assert_eq!(status["thread"], "feature/drop-in");
    assert!(
        status["changes"]["modified"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "tracked.txt"),
        "manual Git commits after bootstrap should register as ahead of the last Heddle capture: {status}"
    );
    assert_eq!(status["recommended_action"], "heddle capture");

    let show = json(temp.path(), &["show", "HEAD", "--json"]);
    assert!(show["change_id"].as_str().is_some());

    let log = json(temp.path(), &["log", "--json"]);
    assert!(
        !log["states"].as_array().unwrap().is_empty(),
        "log should still succeed after plain git commits: {log}"
    );

    let compare = json(temp.path(), &["compare", "HEAD", "HEAD"]);
    assert_eq!(compare["summary"]["total"], 0);

    let ready = json(temp.path(), &["--json", "ready"]);
    assert!(
        ready["thread_state"].is_string(),
        "ready should still produce a valid thread/report surface after plain git commits: {ready}"
    );
}

#[test]
fn git_overlay_matrix_branch_lifecycle_refreshes_import_hints() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    // Import-hint information has moved to `heddle bridge git status
    // --json`; per-command outputs (status, log, show, workspace,
    // thread list) no longer carry it.
    git(&["branch", "support/original"], temp.path());
    let bridge_before = json(temp.path(), &["bridge", "git", "status", "--json"]);
    assert_eq!(
        bridge_before["git_overlay_import_hint"]["missing_branches"][0],
        "support/original"
    );

    git(
        &["branch", "-m", "support/original", "support/renamed"],
        temp.path(),
    );
    let bridge_after_rename = json(temp.path(), &["bridge", "git", "status", "--json"]);
    assert_eq!(
        bridge_after_rename["git_overlay_import_hint"]["missing_branches"][0],
        "support/renamed"
    );

    git(&["branch", "-D", "support/renamed"], temp.path());
    let bridge_after_delete = json(temp.path(), &["bridge", "git", "status", "--json"]);
    assert!(
        bridge_after_delete["git_overlay_import_hint"].is_null(),
        "deleting the extra branch should clear the import hint: {bridge_after_delete}"
    );

    git(&["branch", "support/recreated"], temp.path());
    let bridge_after_recreate = json(temp.path(), &["bridge", "git", "status", "--json"]);
    assert_eq!(
        bridge_after_recreate["git_overlay_import_hint"]["missing_branches"][0],
        "support/recreated"
    );
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

    let thread_list = json(temp.path(), &["thread", "list", "--json"]);
    let threads = thread_list["threads"].as_array().unwrap();
    let alpha = threads
        .iter()
        .find(|thread| thread["name"] == "support/alpha")
        .expect("support/alpha should appear as an auto-adopted branch tip");
    assert_eq!(alpha["history_imported"], false);
    assert!(alpha["git_branch_tip"].as_str().is_some());

    let beta_show = json(temp.path(), &["thread", "show", "support/beta", "--json"]);
    assert_eq!(beta_show["name"], "support/beta");
    assert_eq!(beta_show["history_imported"], false);
    assert!(beta_show["git_branch_tip"].as_str().is_some());

    let workspace = json(temp.path(), &["workspace", "show", "--json"]);
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
            .any(|thread| thread["name"] == "support/alpha")
    );

    // Import-hint information has moved to `heddle bridge git status
    // --json`; per-command outputs no longer carry it.
    let bridge = json(temp.path(), &["bridge", "git", "status", "--json"]);
    assert_eq!(bridge["git_overlay_import_hint"]["missing_branch_count"], 2);
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
        &["thread", "show", "support/imported", "--json"],
    );
    assert_eq!(before["history_imported"], false);

    heddle(&["bridge", "import", "--path", "."], Some(temp.path())).unwrap();

    let after = json(
        temp.path(),
        &["thread", "show", "support/imported", "--json"],
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

    let status = json(temp.path(), &["status", "--json"]);
    assert_eq!(status["thread"], "develop");

    let diagnose = json(temp.path(), &["diagnose", "--json"]);
    assert_eq!(diagnose["thread"]["name"], "develop");

    let thread_list = json(temp.path(), &["thread", "list", "--json"]);
    assert_eq!(thread_list["current"], "develop");

    let workspace = json(temp.path(), &["workspace", "show", "--json"]);
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

    let status = json(temp.path(), &["status", "--json"]);
    assert_eq!(
        status["thread"], "feature/drop-in",
        "after a prior Heddle bootstrap, detached Git HEAD should still preserve the attached Heddle thread lane: {status}"
    );
    assert!(
        status["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "detached.txt")
    );

    let diagnose = json(temp.path(), &["diagnose", "--json"]);
    assert_eq!(diagnose["repository_capability"], "git-overlay");
    assert!(diagnose["git_overlay_import_hint"].is_null());

    let thread_list = json(temp.path(), &["thread", "list", "--json"]);
    assert_eq!(thread_list["current"], "feature/drop-in");

    let workspace = json(temp.path(), &["workspace", "show", "--json"]);
    assert_eq!(workspace["current_thread"], "feature/drop-in");

    let show = json(temp.path(), &["show", "HEAD", "--json"]);
    assert!(show["change_id"].as_str().is_some());

    let log = json(temp.path(), &["log", "--json"]);
    assert!(
        !log["states"].as_array().unwrap().is_empty(),
        "detached HEAD should still have a visible history surface: {log}"
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

    let status = json(temp.path(), &["status", "--json"]);
    assert_git_overlay_basics(&status);
    assert!(
        status["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "detached-tag.txt"),
        "status should remain usable when detached at a tag: {status}"
    );

    let diagnose = json(temp.path(), &["diagnose", "--json"]);
    assert_git_overlay_basics(&diagnose);

    let show = json(temp.path(), &["show", "HEAD", "--json"]);
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
    heddle(&["init"], Some(temp.path())).unwrap();
    let _ = json(temp.path(), &["status", "--json"]);

    std::fs::write(temp.path().join("shared.txt"), "carried modification").unwrap();
    git(&["checkout", "support/carry"], temp.path());
    std::fs::write(temp.path().join("carry.txt"), "branch local").unwrap();

    let status = json(temp.path(), &["status", "--json"]);
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

    let ready = json(temp.path(), &["--json", "ready"]);
    assert_eq!(ready["captured"], true);

    let after_ready = json(temp.path(), &["status", "--json"]);
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

    let compare = json(temp.path(), &["compare", "HEAD", "HEAD"]);
    assert_eq!(compare["summary"]["total"], 0);

    let ready = json(temp.path(), &["--json", "ready"]);
    assert_eq!(ready["thread_state"], "ready");

    let checkpoint = json(temp.path(), &["checkpoint", "-m", "First-run checkpoint"]);
    assert_eq!(checkpoint["summary"], "First-run checkpoint");
    assert_eq!(checkpoint["storage_model"], "git+heddle-sidecar");
    assert!(checkpoint["git_commit"].as_str().is_some());

    let status = json(temp.path(), &["status", "--json"]);
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

    let before = json(temp.path(), &["bridge", "git", "status", "--json"]);
    assert_eq!(before["git_overlay_import_hint"]["missing_branch_count"], 2);

    let import_output = heddle(&["bridge", "import", "--path", "."], Some(temp.path())).unwrap();
    assert!(
        import_output.contains("branches") || import_output.contains("\"branches_synced\""),
        "bridge import should report branch sync activity: {import_output}"
    );

    let after_import = json(temp.path(), &["thread", "list", "--json"]);
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

    let status = json(temp.path(), &["bridge", "git", "status", "--json"]);
    let missing = status["git_overlay_import_hint"]["missing_branches"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert!(
        missing.contains(&"support/alpha-renamed"),
        "renamed imported branch should reappear as missing Git-only evolution: {status}"
    );
    assert!(
        missing.contains(&"support/gamma"),
        "new Git branch after import should appear in import hints: {status}"
    );
    assert!(
        !missing.contains(&"support/beta"),
        "deleted Git branch should not remain in import hints: {status}"
    );
}

#[test]
fn git_overlay_matrix_stale_conflict_ship_blocks_with_guidance() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("conflict.txt"), "base\n").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(
        &["bridge", "git", "import", "--ref", "feature/drop-in"],
        Some(temp.path()),
    )
    .unwrap();
    heddle(&["capture", "-m", "Bootstrap"], Some(temp.path())).unwrap();

    let started = json(
        temp.path(),
        &[
            "--json",
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
    json(temp.path(), &["--json", "commit", "-m", "main change"]);

    let before_ship = json(
        temp.path(),
        &["thread", "show", "feature/conflict", "--json"],
    );
    assert_eq!(before_ship["freshness"], "stale");

    let ship = json(
        temp.path(),
        &["--json", "ship", "--thread", "feature/conflict"],
    );
    assert_eq!(ship["status"], "blocked");
    assert_eq!(ship["checkpointed"], false);
    assert_eq!(ship["integrated"], false);
    assert!(
        ship["next_action"]
            .as_str()
            .unwrap_or("")
            .contains("refresh")
            || ship["next_action"]
                .as_str()
                .unwrap_or("")
                .contains("resolve"),
        "blocked ship should surface the next operator step: {ship}"
    );

    let thread_show = json(
        temp.path(),
        &["thread", "show", "feature/conflict", "--json"],
    );
    assert_eq!(thread_show["thread_state"], "active");
    assert!(
        thread_show["recommended_action"]
            .as_str()
            .unwrap_or("")
            .contains("refresh")
            || thread_show["recommended_action"]
                .as_str()
                .unwrap_or("")
                .contains("resolve")
    );
}

#[test]
fn git_overlay_matrix_reopen_from_different_cwds_preserves_state_and_hints() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/reopen-me"], temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();

    let root_status = json(temp.path(), &["status", "--json"]);
    assert_eq!(root_status["thread"], "feature/drop-in");
    let root_bridge = json(temp.path(), &["bridge", "git", "status", "--json"]);
    assert_eq!(
        root_bridge["git_overlay_import_hint"]["missing_branch_count"],
        1
    );

    let nested = temp.path().join("src/reopen/check");
    std::fs::create_dir_all(&nested).unwrap();
    let nested_workspace = json(&nested, &["workspace", "show", "--json"]);
    assert_eq!(nested_workspace["current_thread"], "feature/drop-in");
    let nested_bridge = json(&nested, &["bridge", "git", "status", "--json"]);
    assert_eq!(
        nested_bridge["git_overlay_import_hint"]["missing_branch_count"],
        1
    );

    std::fs::write(temp.path().join("tracked.txt"), "tracked after reopen").unwrap();
    let ready = json(&nested, &["--json", "ready"]);
    assert_eq!(ready["captured"], true);

    let root_show = json(temp.path(), &["show", "HEAD", "--json"]);
    assert!(root_show["change_id"].as_str().is_some());

    let nested_log = json(&nested, &["log", "--json"]);
    assert!(
        !nested_log["states"].as_array().unwrap().is_empty(),
        "reopened nested cwd should still see persisted history: {nested_log}"
    );

    let root_status_after = json(temp.path(), &["status", "--json"]);
    assert!(
        root_status_after["changes"]["modified"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let root_bridge_after = json(temp.path(), &["bridge", "git", "status", "--json"]);
    assert_eq!(
        root_bridge_after["git_overlay_import_hint"]["missing_branch_count"],
        1
    );
}

#[test]
fn git_overlay_matrix_binary_file_commands_remain_coherent() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("binary.bin"), vec![0u8, 1, 2, 3, 255]).unwrap();
    git_commit_all(temp.path(), "seed binary");

    std::fs::write(temp.path().join("binary.bin"), vec![9u8, 8, 7, 6, 5]).unwrap();

    let status = json(temp.path(), &["status", "--json"]);
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

    let ready = json(temp.path(), &["--json", "ready"]);
    assert_eq!(ready["captured"], true);

    let status_after = json(temp.path(), &["status", "--json"]);
    assert!(
        status_after["changes"]["modified"]
            .as_array()
            .unwrap()
            .is_empty()
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

    std::fs::remove_file(temp.path().join("link.txt")).unwrap();
    symlink("other.txt", temp.path().join("link.txt")).unwrap();

    let status = json(temp.path(), &["status", "--json"]);
    assert!(
        status["changes"]["modified"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "link.txt")
    );

    let ready = json(temp.path(), &["--json", "ready"]);
    assert_eq!(ready["captured"], true);

    let show = json(temp.path(), &["show", "HEAD", "--json"]);
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

    let script = temp.path().join("script.sh");
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let status = json(temp.path(), &["status", "--json"]);
    assert!(
        status["changes"]["modified"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "script.sh")
    );

    let ready = json(temp.path(), &["--json", "ready"]);
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
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(
        &["bridge", "git", "import", "--ref", "feature/drop-in"],
        Some(temp.path()),
    )
    .unwrap();
    heddle(&["capture", "-m", "Bootstrap"], Some(temp.path())).unwrap();

    let started = json(
        temp.path(),
        &[
            "--json",
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
    json(temp.path(), &["--json", "commit", "-m", "advance main"]);

    let before_sync = json(
        temp.path(),
        &["thread", "show", "feature/recover", "--json"],
    );
    assert_eq!(before_sync["freshness"], "stale");

    let sync = json(
        temp.path(),
        &["--json", "sync", "--thread", "feature/recover"],
    );
    assert_eq!(sync["status"], "refreshed");
    assert_eq!(sync["chosen_path"], "refresh");

    let after_sync = json(
        temp.path(),
        &["thread", "show", "feature/recover", "--json"],
    );
    assert_eq!(after_sync["freshness"], "current");

    let ship = json(
        temp.path(),
        &["--json", "ship", "--thread", "feature/recover"],
    );
    assert_eq!(ship["status"], "shipped");
    assert_eq!(ship["checkpointed"], true);
    assert!(ship["git_commit"].as_str().is_some());
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

    let status = json(temp.path(), &["status", "--json"]);
    assert_eq!(status["thread"], "feature/drop-in");
    assert_eq!(status["recommended_action"], "heddle capture");
    assert!(
        status["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "main.txt")
    );
    assert!(
        status["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "side.txt")
    );

    let log = json(temp.path(), &["log", "--json"]);
    assert!(
        !log["states"].as_array().unwrap().is_empty(),
        "log should stay coherent after a manual Git merge commit: {log}"
    );

    let compare = json(temp.path(), &["compare", "HEAD", "HEAD"]);
    assert_eq!(compare["summary"]["total"], 0);

    let ready = json(temp.path(), &["--json", "ready"]);
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

    let import_output = heddle(&["bridge", "import", "--path", "."], Some(temp.path())).unwrap();
    assert!(
        import_output.contains("branches") || import_output.contains("\"branches_synced\""),
        "bridge import should report branch sync activity: {import_output}"
    );

    let threads_after_import = json(temp.path(), &["thread", "list", "--json"]);
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

    let status = json(temp.path(), &["bridge", "git", "status", "--json"]);
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

    let bridge = json(temp.path(), &["bridge", "git", "status", "--json"]);
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

    let status = json(temp.path(), &["bridge", "git", "status", "--json"]);
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

    let bridge_again = json(temp.path(), &["bridge", "git", "status", "--json"]);
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

    let status = json(temp.path(), &["status", "--json"]);
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
    heddle(&["init"], Some(rebase_repo.path())).unwrap();
    let _ = json(rebase_repo.path(), &["status", "--json"]);

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

    let status = json(rebase_repo.path(), &["status", "--json"]);
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

    let diagnose = json(rebase_repo.path(), &["diagnose", "--json"]);
    assert_eq!(diagnose["repository_capability"], "git-overlay");

    let worktree = json(rebase_repo.path(), &["workspace", "show", "--json"]);
    assert_eq!(worktree["repository_capability"], "git-overlay");

    git(&["rebase", "--abort"], rebase_repo.path());

    let cherry_repo = TempDir::new().unwrap();
    init_git_repo_with_branch(cherry_repo.path(), "feature/drop-in");
    std::fs::write(cherry_repo.path().join("conflict.txt"), "base\n").unwrap();
    git_commit_all(cherry_repo.path(), "seed branch");
    heddle(&["init"], Some(cherry_repo.path())).unwrap();
    let _ = json(cherry_repo.path(), &["status", "--json"]);

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

    let cherry_pick = Command::new("git")
        .args(["cherry-pick", &cherry_commit])
        .current_dir(cherry_repo.path())
        .output()
        .expect("git cherry-pick should run");
    assert!(
        !cherry_pick.status.success(),
        "expected conflicting cherry-pick to stop for manual resolution"
    );

    let cherry_status = json(cherry_repo.path(), &["status", "--json"]);
    assert_eq!(cherry_status["thread"], "feature/drop-in");
    assert!(
        cherry_status["changes"]["modified"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "conflict.txt"),
        "status should stay coherent during cherry-pick conflict: {cherry_status}"
    );

    let cherry_show = json(cherry_repo.path(), &["show", "HEAD", "--json"]);
    assert!(cherry_show["change_id"].as_str().is_some());

    git(&["cherry-pick", "--abort"], cherry_repo.path());
}

#[test]
fn git_overlay_matrix_stale_ship_manual_resolution_then_retry_ships() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("conflict.txt"), "base\n").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(&["capture", "-m", "Bootstrap"], Some(temp.path())).unwrap();

    let started = json(
        temp.path(),
        &[
            "--json",
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
        &["--json", "ship", "--thread", "feature/manual-recover"],
    );
    assert_eq!(blocked["status"], "blocked");

    std::fs::write(
        thread_path.join("conflict.txt"),
        "main change\nthread change\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "manual resolve"], Some(&thread_path)).unwrap();

    let refresh_output = heddle(
        &["thread", "refresh", "feature/manual-recover"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        refresh_output.contains("Refreshed thread 'feature/manual-recover'")
            || refresh_output.contains("\"message\":\"Refreshed thread"),
        "manual resolution loop should support an explicit thread refresh: {refresh_output}"
    );

    let resolve_output = heddle(
        &["thread", "resolve", "feature/manual-recover"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        resolve_output.contains("\"status\":\"completed\"")
            || resolve_output.contains("\"status\": \"completed\"")
            || resolve_output
                .contains("\"recommended_action\":\"heddle ship --thread feature/manual-recover\""),
        "thread resolve should surface the ship retry step after refresh: {resolve_output}"
    );

    let retry_ship = json(
        temp.path(),
        &["--json", "ship", "--thread", "feature/manual-recover"],
    );
    assert_eq!(retry_ship["status"], "shipped");
    assert_eq!(retry_ship["checkpointed"], true);
    assert!(retry_ship["git_commit"].as_str().is_some());
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

    heddle(&["init"], Some(&worktree_path)).unwrap();
    std::fs::write(worktree_path.join("native.txt"), "native worktree\n").unwrap();

    let status = json(&worktree_path, &["status", "--json"]);
    assert_eq!(status["thread"], "support/native-worktree");
    assert_eq!(status["repository_capability"], "git-overlay");
    assert!(
        status["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "native.txt")
    );

    let workspace = json(&worktree_path, &["workspace", "show", "--json"]);
    assert_eq!(workspace["current_thread"], "support/native-worktree");

    let ready = json(&worktree_path, &["--json", "ready"]);
    assert_eq!(ready["captured"], true);
}

#[test]
fn git_overlay_matrix_current_branch_rename_updates_active_thread_views() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_branch(temp.path(), "feature/drop-in");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle(&["init"], Some(temp.path())).unwrap();
    let _ = json(temp.path(), &["status", "--json"]);

    git(&["branch", "-m", "feature/renamed-current"], temp.path());

    let status = json(temp.path(), &["status", "--json"]);
    assert_eq!(status["thread"], "feature/renamed-current");

    let thread_list = json(temp.path(), &["thread", "list", "--json"]);
    assert_eq!(thread_list["current"], "feature/renamed-current");

    let workspace = json(temp.path(), &["workspace", "show", "--json"]);
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

    let status = json(temp.path(), &["bridge", "git", "status", "--json"]);
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
    let _ = json(rebase_repo.path(), &["status", "--json"]);

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

    let status = json(rebase_repo.path(), &["status", "--json"]);
    assert_eq!(status["operation"]["scope"], "git");
    assert_eq!(status["operation"]["kind"], "rebase");
    assert_eq!(status["operation"]["next_action"], "heddle continue");
    let diagnose = json(rebase_repo.path(), &["diagnose", "--json"]);
    assert_eq!(diagnose["operation"]["kind"], "rebase");
    let workspace = json(rebase_repo.path(), &["workspace", "show", "--json"]);
    assert_eq!(workspace["operation"]["kind"], "rebase");
    let thread_list = json(rebase_repo.path(), &["thread", "list", "--json"]);
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
    let _ = json(revert_repo.path(), &["status", "--json"]);
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

    let revert_status = json(revert_repo.path(), &["status", "--json"]);
    assert_eq!(revert_status["operation"]["kind"], "revert");
    assert_eq!(revert_status["operation"]["next_action"], "heddle continue");
    git(&["revert", "--abort"], revert_repo.path());

    let bisect_repo = TempDir::new().unwrap();
    init_git_repo_with_branch(bisect_repo.path(), "feature/drop-in");
    std::fs::write(bisect_repo.path().join("tracked.txt"), "base\n").unwrap();
    git_commit_all(bisect_repo.path(), "seed branch");
    let _ = json(bisect_repo.path(), &["status", "--json"]);
    heddle(&["bisect", "start"], Some(bisect_repo.path())).unwrap();
    let bisect_status = json(bisect_repo.path(), &["status", "--json"]);
    assert_eq!(bisect_status["operation"]["scope"], "heddle");
    assert_eq!(bisect_status["operation"]["kind"], "bisect");
    assert_eq!(
        bisect_status["operation"]["next_action"],
        "heddle bisect good <state> or heddle bisect bad <state>"
    );
    heddle(&["bisect", "reset"], Some(bisect_repo.path())).unwrap();
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
    heddle(&["init"], Some(temp.path())).unwrap();
    let _ = json(temp.path(), &["status", "--json"]);

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
    heddle(&["init"], Some(&worktree_path)).unwrap();
    let worktree_status = json(&worktree_path, &["status", "--json"]);
    assert_eq!(worktree_status["thread"], "support/native-worktree");
    assert!(worktree_status["remote_tracking"].is_null());

    git(
        &["checkout", "-b", "support/renamed-switch"],
        &worktree_path,
    );
    std::fs::write(worktree_path.join("renamed.txt"), "renamed branch\n").unwrap();
    let switched = json(&worktree_path, &["workspace", "show", "--json"]);
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

    let root_status = json(temp.path(), &["status", "--json"]);
    assert_eq!(root_status["thread"], "feature/drop-in");
    assert_eq!(root_status["remote_tracking"]["branch"], "feature/drop-in");
    assert_eq!(root_status["remote_tracking"]["behind"], 1);
    assert_eq!(
        root_status["remote_tracking"]["next_action"],
        "git pull --rebase"
    );

    let thread_list = json(temp.path(), &["thread", "list", "--json"]);
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

    let merge_output = heddle(&["merge", "main"], Some(temp.path())).unwrap();
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
        envelope["error"].as_str().is_some_and(|error| error
            .contains("merge is already in progress")
            && error.contains("Unsafe:")
            && error.contains("Would change:")
            && error.contains("Preserved:")
            && error.contains("Primary recovery:")),
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

    let continued = json(temp.path(), &["--json", "continue"]);
    assert_eq!(continued["status"], "continued");

    let status_after_continue = json(temp.path(), &["status", "--json"]);
    assert!(status_after_continue["operation"].is_null());

    let git_repo = TempDir::new().unwrap();
    init_git_repo_with_branch(git_repo.path(), "feature/drop-in");
    std::fs::write(git_repo.path().join("base.txt"), "base\n").unwrap();
    git_commit_all(git_repo.path(), "seed branch");
    heddle(&["init"], Some(git_repo.path())).unwrap();
    let _ = json(git_repo.path(), &["status", "--json"]);
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

    let aborted = json(git_repo.path(), &["--json", "abort"]);
    assert_eq!(aborted["status"], "aborted");
    let status_after_abort = json(git_repo.path(), &["status", "--json"]);
    assert!(status_after_abort["operation"].is_null());
}

#[test]
fn git_overlay_matrix_rebase_noop_defers_up_to_date_claim_to_trust() {
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
    heddle(&["init"], Some(temp.path())).unwrap();
    let _ = json(temp.path(), &["status", "--json"]);

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

    let trust_before = json(temp.path(), &["trust", "--json"]);
    assert_eq!(trust_before["trusted"], false);
    assert_eq!(trust_before["trust"]["remote_drift"], "remote_behind");

    let rebase = json(temp.path(), &["--json", "rebase", "feature/drop-in"]);
    assert_eq!(rebase["status"], "blocked");
    assert_eq!(rebase["reason"], "repository_trust");
    assert_eq!(rebase["trust"]["trusted"], false);
    assert_eq!(rebase["trust"]["remote_drift"], "remote_behind");
    assert_eq!(rebase["recommended_action"], "heddle pull");
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
    heddle(&["init"], Some(temp.path())).unwrap();
    let _ = json(temp.path(), &["status", "--json"]);

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

    let status_before = json(temp.path(), &["status", "--json"]);
    assert_eq!(status_before["remote_tracking"]["behind"], 1);
    assert_eq!(status_before["recommended_action"], "heddle pull");

    let diagnose_before = json(temp.path(), &["diagnose", "--json"]);
    assert_eq!(
        diagnose_before["health"]["recommended_action"],
        "heddle pull"
    );

    let sync = json(temp.path(), &["--json", "sync"]);
    assert_eq!(sync["status"], "synced");

    let trust_after_sync = json(temp.path(), &["trust", "--json"]);
    assert_eq!(
        trust_after_sync["trusted"], true,
        "sync may report synced only after the shared trust engine is clean: {trust_after_sync}"
    );

    let status_after = json(temp.path(), &["status", "--json"]);
    assert!(status_after["remote_tracking"].is_null());
}

#[test]
fn git_overlay_matrix_continue_handles_each_supported_operation_state() {
    // Heddle merge: unresolved conflicts should block, then continue should finish once resolved.
    let heddle_merge = TempDir::new().unwrap();
    init_heddle_conflict_repo(heddle_merge.path());
    let _ = heddle(&["merge", "main"], Some(heddle_merge.path())).unwrap();

    let blocked_continue = json(heddle_merge.path(), &["--json", "continue"]);
    assert_eq!(blocked_continue["status"], "blocked");
    assert_eq!(blocked_continue["next_action"], "heddle resolve --list");
    assert_eq!(
        blocked_continue["recommended_action"],
        "heddle resolve conflict.txt"
    );

    heddle(&["resolve", "--all", "--ours"], Some(heddle_merge.path())).unwrap();
    let continued_merge = json(heddle_merge.path(), &["--json", "continue"]);
    assert_eq!(continued_merge["status"], "continued");
    assert!(json(heddle_merge.path(), &["status", "--json"])["operation"].is_null());

    // Git merge: continue should run `git merge --continue`.
    let git_merge = TempDir::new().unwrap();
    init_git_repo_with_branch(git_merge.path(), "feature/drop-in");
    std::fs::write(git_merge.path().join("conflict.txt"), "base\n").unwrap();
    git_commit_all(git_merge.path(), "seed branch");
    heddle(&["init"], Some(git_merge.path())).unwrap();
    let _ = json(git_merge.path(), &["status", "--json"]);
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
    let continued_git_merge = json(git_merge.path(), &["--json", "continue"]);
    assert_eq!(continued_git_merge["status"], "continued");
    assert!(json(git_merge.path(), &["status", "--json"])["operation"].is_null());

    // Git cherry-pick: continue should run `git cherry-pick --continue`.
    let git_cherry = TempDir::new().unwrap();
    init_git_repo_with_branch(git_cherry.path(), "feature/drop-in");
    std::fs::write(git_cherry.path().join("conflict.txt"), "base\n").unwrap();
    git_commit_all(git_cherry.path(), "seed branch");
    let _ = json(git_cherry.path(), &["status", "--json"]);
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
    let continued_git_cherry = json(git_cherry.path(), &["--json", "continue"]);
    assert_eq!(continued_git_cherry["status"], "continued");
    assert!(json(git_cherry.path(), &["status", "--json"])["operation"].is_null());

    // Git revert: continue should run `git revert --continue`.
    let git_revert = TempDir::new().unwrap();
    init_git_repo_with_branch(git_revert.path(), "feature/drop-in");
    std::fs::write(git_revert.path().join("tracked.txt"), "base\n").unwrap();
    git_commit_all(git_revert.path(), "seed branch");
    let _ = json(git_revert.path(), &["status", "--json"]);
    std::fs::write(git_revert.path().join("tracked.txt"), "main change\n").unwrap();
    git_commit_all(git_revert.path(), "main change");
    let revert = Command::new("git")
        .args(["revert", "--no-commit", "HEAD"])
        .current_dir(git_revert.path())
        .output()
        .expect("git revert should run");
    assert!(revert.status.success());
    git(&["add", "tracked.txt"], git_revert.path());
    let continued_git_revert = json(git_revert.path(), &["--json", "continue"]);
    assert_eq!(continued_git_revert["status"], "continued");
    assert!(json(git_revert.path(), &["status", "--json"])["operation"].is_null());

    // Bisect states should remain intentionally blocked under continue.
    let heddle_bisect = TempDir::new().unwrap();
    init_git_repo_with_branch(heddle_bisect.path(), "feature/drop-in");
    std::fs::write(heddle_bisect.path().join("tracked.txt"), "base\n").unwrap();
    git_commit_all(heddle_bisect.path(), "seed branch");
    let _ = json(heddle_bisect.path(), &["status", "--json"]);
    heddle(&["bisect", "start"], Some(heddle_bisect.path())).unwrap();
    let blocked_heddle_bisect = json(heddle_bisect.path(), &["--json", "continue"]);
    assert_eq!(blocked_heddle_bisect["status"], "blocked");
    assert_eq!(
        blocked_heddle_bisect["recommended_action"],
        "heddle bisect good <state> or heddle bisect bad <state>"
    );

    let git_bisect = TempDir::new().unwrap();
    init_git_repo_with_branch(git_bisect.path(), "feature/drop-in");
    std::fs::write(git_bisect.path().join("tracked.txt"), "base\n").unwrap();
    git_commit_all(git_bisect.path(), "seed branch");
    std::fs::write(git_bisect.path().join("tracked.txt"), "middle\n").unwrap();
    git_commit_all(git_bisect.path(), "middle change");
    std::fs::write(git_bisect.path().join("tracked.txt"), "bad\n").unwrap();
    git_commit_all(git_bisect.path(), "bad change");
    let _ = json(git_bisect.path(), &["status", "--json"]);
    git(&["bisect", "start"], git_bisect.path());
    git(&["bisect", "bad"], git_bisect.path());
    git(&["bisect", "good", "HEAD~2"], git_bisect.path());
    let blocked_git_bisect = json(git_bisect.path(), &["--json", "continue"]);
    assert_eq!(blocked_git_bisect["status"], "blocked");
    assert_eq!(
        blocked_git_bisect["recommended_action"],
        "git bisect good or git bisect bad"
    );
}

#[test]
fn git_overlay_matrix_abort_handles_each_supported_operation_state() {
    let heddle_merge = TempDir::new().unwrap();
    init_heddle_conflict_repo(heddle_merge.path());
    let _ = heddle(&["merge", "main"], Some(heddle_merge.path())).unwrap();
    let aborted_heddle_merge = json(heddle_merge.path(), &["--json", "abort"]);
    assert_eq!(aborted_heddle_merge["status"], "aborted");
    assert!(json(heddle_merge.path(), &["status", "--json"])["operation"].is_null());

    let heddle_bisect = TempDir::new().unwrap();
    init_git_repo_with_branch(heddle_bisect.path(), "feature/drop-in");
    std::fs::write(heddle_bisect.path().join("tracked.txt"), "base\n").unwrap();
    git_commit_all(heddle_bisect.path(), "seed branch");
    let _ = json(heddle_bisect.path(), &["status", "--json"]);
    heddle(&["bisect", "start"], Some(heddle_bisect.path())).unwrap();
    let aborted_heddle_bisect = json(heddle_bisect.path(), &["--json", "abort"]);
    assert_eq!(aborted_heddle_bisect["status"], "aborted");
    assert!(json(heddle_bisect.path(), &["status", "--json"])["operation"].is_null());

    let git_rebase = TempDir::new().unwrap();
    init_git_repo_with_branch(git_rebase.path(), "feature/drop-in");
    std::fs::write(git_rebase.path().join("clash.txt"), "base\n").unwrap();
    git_commit_all(git_rebase.path(), "seed branch");
    let _ = json(git_rebase.path(), &["status", "--json"]);
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
    let aborted_git_rebase = json(git_rebase.path(), &["--json", "abort"]);
    assert_eq!(aborted_git_rebase["status"], "aborted");
    assert!(json(git_rebase.path(), &["status", "--json"])["operation"].is_null());

    let git_merge = TempDir::new().unwrap();
    init_git_repo_with_branch(git_merge.path(), "feature/drop-in");
    std::fs::write(git_merge.path().join("conflict.txt"), "base\n").unwrap();
    git_commit_all(git_merge.path(), "seed branch");
    heddle(&["init"], Some(git_merge.path())).unwrap();
    let _ = json(git_merge.path(), &["status", "--json"]);
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
    let aborted_git_merge = json(git_merge.path(), &["--json", "abort"]);
    assert_eq!(aborted_git_merge["status"], "aborted");
    assert!(json(git_merge.path(), &["status", "--json"])["operation"].is_null());

    let git_cherry = TempDir::new().unwrap();
    init_git_repo_with_branch(git_cherry.path(), "feature/drop-in");
    std::fs::write(git_cherry.path().join("conflict.txt"), "base\n").unwrap();
    git_commit_all(git_cherry.path(), "seed branch");
    let _ = json(git_cherry.path(), &["status", "--json"]);
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
    let aborted_git_cherry = json(git_cherry.path(), &["--json", "abort"]);
    assert_eq!(aborted_git_cherry["status"], "aborted");
    assert!(json(git_cherry.path(), &["status", "--json"])["operation"].is_null());

    let git_revert = TempDir::new().unwrap();
    init_git_repo_with_branch(git_revert.path(), "feature/drop-in");
    std::fs::write(git_revert.path().join("tracked.txt"), "base\n").unwrap();
    git_commit_all(git_revert.path(), "seed branch");
    let _ = json(git_revert.path(), &["status", "--json"]);
    std::fs::write(git_revert.path().join("tracked.txt"), "main change\n").unwrap();
    git_commit_all(git_revert.path(), "main change");
    let revert = Command::new("git")
        .args(["revert", "--no-commit", "HEAD"])
        .current_dir(git_revert.path())
        .output()
        .expect("git revert should run");
    assert!(revert.status.success());
    let aborted_git_revert = json(git_revert.path(), &["--json", "abort"]);
    assert_eq!(aborted_git_revert["status"], "aborted");
    assert!(json(git_revert.path(), &["status", "--json"])["operation"].is_null());

    let git_bisect = TempDir::new().unwrap();
    init_git_repo_with_branch(git_bisect.path(), "feature/drop-in");
    std::fs::write(git_bisect.path().join("tracked.txt"), "base\n").unwrap();
    git_commit_all(git_bisect.path(), "seed branch");
    std::fs::write(git_bisect.path().join("tracked.txt"), "middle\n").unwrap();
    git_commit_all(git_bisect.path(), "middle change");
    std::fs::write(git_bisect.path().join("tracked.txt"), "bad\n").unwrap();
    git_commit_all(git_bisect.path(), "bad change");
    let _ = json(git_bisect.path(), &["status", "--json"]);
    git(&["bisect", "start"], git_bisect.path());
    git(&["bisect", "bad"], git_bisect.path());
    git(&["bisect", "good", "HEAD~2"], git_bisect.path());
    let aborted_git_bisect = json(git_bisect.path(), &["--json", "abort"]);
    assert_eq!(aborted_git_bisect["status"], "aborted");
    assert!(json(git_bisect.path(), &["status", "--json"])["operation"].is_null());
}

#[test]
fn git_overlay_matrix_operator_states_survive_reopen_and_keep_guidance_consistent() {
    let temp = TempDir::new().unwrap();
    init_heddle_conflict_repo(temp.path());
    let _ = heddle(&["merge", "main"], Some(temp.path())).unwrap();

    let status = json(temp.path(), &["status", "--json"]);
    let diagnose = json(temp.path(), &["diagnose", "--json"]);
    let thread_show = json(temp.path(), &["thread", "show", "feature", "--json"]);
    let workspace = json(temp.path(), &["workspace", "show", "--json"]);

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
    let status_reopened = json(&nested, &["status", "--json"]);
    let workspace_reopened = json(&nested, &["workspace", "show", "--json"]);
    assert_eq!(status_reopened["operation"]["kind"], "merge");
    assert_eq!(status_reopened["recommended_action"], "heddle continue");
    assert_eq!(workspace_reopened["recommended_action"], "heddle continue");
}

#[test]
fn git_overlay_matrix_continue_retry_loops_block_then_succeed_after_resolution() {
    let heddle_merge = TempDir::new().unwrap();
    init_heddle_conflict_repo(heddle_merge.path());
    let _ = heddle(&["merge", "main"], Some(heddle_merge.path())).unwrap();
    let blocked = json(heddle_merge.path(), &["--json", "continue"]);
    assert_eq!(blocked["status"], "blocked");
    heddle(&["resolve", "--all", "--ours"], Some(heddle_merge.path())).unwrap();
    let continued = json(heddle_merge.path(), &["--json", "continue"]);
    assert_eq!(continued["status"], "continued");

    let git_merge = TempDir::new().unwrap();
    init_git_repo_with_branch(git_merge.path(), "feature/drop-in");
    std::fs::write(git_merge.path().join("conflict.txt"), "base\n").unwrap();
    git_commit_all(git_merge.path(), "seed branch");
    heddle(&["init"], Some(git_merge.path())).unwrap();
    let _ = json(git_merge.path(), &["status", "--json"]);
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
    let blocked_git = json(git_merge.path(), &["status", "--json"]);
    assert_eq!(blocked_git["operation"]["kind"], "merge", "{blocked_git}");
    std::fs::write(git_merge.path().join("conflict.txt"), "main merge\n").unwrap();
    git(&["add", "conflict.txt"], git_merge.path());
    let continued_git = json(git_merge.path(), &["--json", "continue"]);
    assert_eq!(continued_git["status"], "continued");
}
