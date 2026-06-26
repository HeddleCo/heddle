// SPDX-License-Identifier: Apache-2.0
use cli::config::UserConfig;
use objects::{object::ThreadName, store::ObjectStore};

use super::*;

fn init_git_repo(path: &std::path::Path) {
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
        .args(["checkout", "-b", "feature/drop-in"])
        .current_dir(path)
        .status()
        .expect("git checkout -b should run");
    assert!(status.success());
}

fn git_commit_all(path: &std::path::Path, message: &str) {
    let status = Command::new("git")
        .args(["add", "."])
        .current_dir(path)
        .status()
        .expect("git add should run");
    assert!(status.success());

    let status = Command::new("git")
        .args(["commit", "-m", message])
        .current_dir(path)
        .status()
        .expect("git commit should run");
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

fn heddle_adopt(path: &std::path::Path) {
    heddle(&["adopt"], Some(path)).unwrap();
}

/// Pipe a patch into `git <args>` (e.g. `["apply", "--check"]`) run in
/// `dir` and return the captured output. Shared by the round-trip tests
/// that prove `heddle diff --patch` produces a body real git accepts.
fn run_git_apply(dir: &std::path::Path, patch: &str, args: &[&str]) -> Output {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git apply should spawn");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch.as_bytes())
        .unwrap();
    child.wait_with_output().expect("git apply should finish")
}

fn git_apply(dir: &std::path::Path, patch: &str) {
    let out = run_git_apply(dir, patch, &["apply"]);
    assert!(
        out.status.success(),
        "git apply must accept the patch;\npatch=\n{patch}\nstderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn json_stdout(output: &Output, context: &str) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "{context} should emit JSON on stdout: {err}; stdout={}; stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// Mutation `--output json` replies no longer embed `verification`
/// (the verification-claim gate still consults it in-memory, but it
/// is omitted from the wire to keep mutation replies focused).
/// Some integration tests pattern-match on the field; this helper
/// invokes `heddle verify --output json` after the fact and grafts
/// the proof back onto the test fixture so the existing assertions
/// keep working without per-call rewrites. Real consumers see the
/// field omitted.
fn inject_post_verification_at(cwd: &std::path::Path, mut value: Value) -> Value {
    let obj = match value.as_object_mut() {
        Some(obj) => obj,
        None => return value,
    };
    if obj.contains_key("verification") {
        return value;
    }
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

#[test]
fn test_cli_capture_blocks_large_git_overlay_deletion_without_force() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::create_dir_all(temp.path().join("web")).unwrap();
    for index in 0..30 {
        std::fs::write(
            temp.path().join("web").join(format!("file-{index}.txt")),
            "tracked",
        )
        .unwrap();
    }
    git_commit_all(temp.path(), "seed web tree");
    heddle_adopt(temp.path());

    std::fs::remove_dir_all(temp.path().join("web")).unwrap();
    let error = heddle(&["capture", "-m", "remove web"], Some(temp.path()))
        .expect_err("large deletion capture should require --force");
    assert!(
        error.contains("Large capture safety check"),
        "large capture should explain the guardrail and escape hatch: {error}"
    );

    let json_refusal = heddle_output(
        &["--output", "json", "capture", "-m", "remove web"],
        Some(temp.path()),
    )
    .expect("large deletion capture should run and refuse");
    assert!(
        !json_refusal.status.success(),
        "large deletion capture should require --force"
    );
    let stderr = str::from_utf8(&json_refusal.stderr).expect("stderr should be utf8");
    let envelope: Value = serde_json::from_str(stderr.trim()).expect("stderr should be JSON");
    assert_eq!(envelope["kind"], "large_capture_requires_force");
    assert!(
        envelope.get("code").is_none(),
        "`kind` is the envelope's only discriminator; the redundant `code` \
         duplicate was dropped pre-1.0 (HeddleCo/heddle#647): {envelope}"
    );
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("Large capture safety check")),
        "JSON error should carry concise refusal text: {envelope}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle capture --force")),
        "JSON hint should name the force retry: {envelope}"
    );
    assert_json_recovery_advice_fields(&envelope, stderr);

    let forced = heddle(
        &["capture", "--force", "-m", "remove web intentionally"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        forced.contains("Captured state"),
        "forced large capture should proceed: {forced}"
    );
}

#[test]
fn test_cli_capture_refuses_noop_worktree() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("steady.txt"), "steady\n").unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();
    let before = heddle(&["show", "HEAD", "--output", "json"], Some(temp.path())).unwrap();
    let before: Value = serde_json::from_str(&before).expect("show HEAD should be JSON");
    let before_id = before["change_id"].as_str().expect("show HEAD change id");

    let output = heddle_output(
        &["--output", "json", "capture", "-m", "noop"],
        Some(temp.path()),
    )
    .expect("heddle capture noop should run and refuse");
    assert!(
        !output.status.success(),
        "noop capture must refuse instead of minting a same-tree state"
    );
    let stderr = str::from_utf8(&output.stderr).expect("stderr should be utf8");
    let envelope: Value = serde_json::from_str(stderr.trim()).expect("stderr should be JSON");
    assert_eq!(envelope["kind"], "nothing_to_commit");

    let after = heddle(&["show", "HEAD", "--output", "json"], Some(temp.path())).unwrap();
    let after: Value = serde_json::from_str(&after).expect("show HEAD should be JSON");
    assert_eq!(after["change_id"].as_str(), Some(before_id));
}

fn seed_git_history(path: &std::path::Path, commit_count: usize) {
    for revision in 0..commit_count {
        std::fs::write(
            path.join("tracked.txt"),
            format!("tracked revision {revision}"),
        )
        .unwrap();
        git_commit_all(path, &format!("seed revision {revision}"));
    }
}

#[test]
fn test_cli_adopt_human_progress_and_json_cleanliness() {
    let human = TempDir::new().unwrap();
    init_git_repo(human.path());
    seed_git_history(human.path(), 3);

    let output = heddle(&["--output", "text", "adopt"], Some(human.path())).unwrap();
    assert!(
        output.contains("Importing Git history:")
            && output.contains("[1/3] scanning refs")
            && output.contains("[2/3] importing commits")
            && output.contains("[2/3] checking Heddle notes")
            && output.contains("[2/3] ordering commits")
            && output.contains("[3/3] writing refs")
            && output.contains("[done] imported Git history"),
        "human adopt should show import phases: {output}"
    );

    let json = TempDir::new().unwrap();
    init_git_repo(json.path());
    seed_git_history(json.path(), 3);
    let output = heddle_output(&["--output", "json", "adopt"], Some(json.path()))
        .expect("json adopt should run");
    assert!(output.status.success());
    let stdout = str::from_utf8(&output.stdout).unwrap();
    assert!(
        !stdout.contains("Importing Git history") && !stdout.contains("[1/3]"),
        "json adopt stdout should not include human progress: {stdout}"
    );
    let parsed = json_stdout(&output, "adopt json");
    assert_eq!(parsed["output_kind"], "adopt");
    assert_eq!(parsed["status"], "completed");
}

#[test]
fn test_cli_adopt_tag_output_does_not_claim_branch_adoption() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["tag", "v1.0.0"], temp.path());

    let output = heddle(
        &["--output", "text", "adopt", "--ref", "v1.0.0"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        output.contains("Heddle imported the requested Git history")
            && output.contains("Imported refs: v1.0.0")
            && output.contains("Branches ready: 0")
            && output.contains("Tags ready: 1"),
        "tag-scoped adoption should describe a tag import: {output}"
    );
    assert!(
        !output.contains("Heddle adopted the requested Git history")
            && !output.contains("Adopted: v1.0.0")
            && !output.contains("Branches ready: 1"),
        "tag-scoped adoption should not imply branch adoption: {output}"
    );
}

#[test]
fn test_cli_adopt_partial_divergence_failure_preserves_state_and_one_recovery() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
    git_commit_all(temp.path(), "base");
    heddle(&["adopt", "--ref", "feature/drop-in"], Some(temp.path())).unwrap();

    std::fs::write(temp.path().join("tracked.txt"), "heddle side\n").unwrap();
    heddle(&["capture", "-m", "heddle side"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "git side\n").unwrap();
    git_commit_all(temp.path(), "git side");

    let output = heddle_output(
        &["--output", "json", "adopt", "--ref", "feature/drop-in"],
        Some(temp.path()),
    )
    .expect("diverged adopt should run and fail");
    assert!(!output.status.success(), "diverged adopt should fail");
    let stdout = str::from_utf8(&output.stdout).unwrap();
    assert!(
        !stdout.contains("Importing Git history") && !stdout.contains("[1/3]"),
        "json failure should not include human progress on stdout: {stdout}"
    );
    let stderr = str::from_utf8(&output.stderr).unwrap();
    let envelope: Value = serde_json::from_str(stderr.trim()).unwrap_or_else(|err| {
        panic!("adopt failure should emit JSON recovery advice: {err}; stderr={stderr}")
    });
    assert_eq!(envelope["kind"], "git_heddle_thread_diverged");
    assert!(
        envelope["preserved"]
            .as_str()
            .is_some_and(|preserved| preserved.contains("imported commit states")
                && preserved.contains("Git/Heddle mapping records")),
        "partial import failure should disclose preserved partial state: {envelope}"
    );
    assert_eq!(
        envelope["primary_command"],
        "heddle bridge git reconcile --ref feature/drop-in --preview"
    );
    assert_eq!(
        envelope["recovery_commands"],
        serde_json::json!(["heddle bridge git reconcile --ref feature/drop-in --preview"])
    );
}

#[test]
fn test_cli_init_creates_repository() {
    let temp = TempDir::new().unwrap();

    let result = heddle(&["init"], Some(temp.path()));
    assert!(result.is_ok(), "Failed to init: {:?}", result.err());

    let heddle_dir = temp.path().join(".heddle");
    assert!(heddle_dir.exists(), ".heddle directory should exist");
    assert!(
        heddle_dir.join("config.toml").exists(),
        "config.toml should exist"
    );
    assert!(heddle_dir.join("HEAD").exists(), "HEAD should exist");
    assert!(
        heddle_dir.join("objects").exists(),
        "objects directory should exist"
    );
}

#[test]
fn test_cli_init_honors_global_repo_path() {
    let temp = TempDir::new().unwrap();
    let cwd = temp.path().join("cwd");
    let target = temp.path().join("target repo");
    std::fs::create_dir_all(&cwd).unwrap();

    let output = heddle(
        &["--repo", target.to_str().expect("utf8 path"), "init"],
        Some(&cwd),
    )
    .expect("init with --repo should succeed");

    assert!(
        output.contains(target.join(".heddle").to_str().expect("utf8 path")),
        "init should report the requested repo path: {output}"
    );
    assert!(
        target.join(".heddle/config.toml").exists(),
        "init must create .heddle under --repo"
    );
    assert!(
        !cwd.join(".heddle").exists(),
        "init must not silently initialize the process cwd when --repo is set"
    );
}

#[test]
fn test_cli_init_rejects_conflicting_repo_and_positional_paths() {
    let temp = TempDir::new().unwrap();
    let cwd = temp.path().join("cwd");
    let repo_path = temp.path().join("repo-a");
    let positional = temp.path().join("repo-b");
    std::fs::create_dir_all(&cwd).unwrap();

    let output = heddle_output(
        &[
            "--repo",
            repo_path.to_str().expect("utf8 path"),
            "init",
            positional.to_str().expect("utf8 path"),
        ],
        Some(&cwd),
    )
    .expect("invoke heddle init");

    assert!(
        !output.status.success(),
        "conflicting init paths should fail before side effects"
    );
    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    assert!(
        stdout.is_empty(),
        "failure should not print success output: {stdout}"
    );
    assert!(
        stderr.contains("positional path") && stderr.contains("--repo"),
        "error should explain the conflicting path inputs: {stderr}"
    );
    assert!(!repo_path.join(".heddle").exists());
    assert!(!positional.join(".heddle").exists());
}

#[test]
fn test_cli_init_fails_on_existing_repo() {
    let temp = TempDir::new().unwrap();
    assert!(heddle(&["init"], Some(temp.path())).is_ok());
    assert!(heddle(&["init"], Some(temp.path())).is_err());
}

#[test]
fn test_cli_init_in_git_repo_bootstraps_sidecar() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());

    let output = heddle(&["init"], Some(temp.path())).unwrap();
    assert!(
        output.contains("Heddle data"),
        "expected user-facing Heddle data language: {output}"
    );

    let status = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&status).unwrap();
    assert_eq!(parsed["repository_capability"], "git-overlay");
    assert_eq!(parsed["storage_model"], "git+heddle-sidecar");
}

/// heddle#644: a successful non-quickstart init on an empty directory
/// must end with a next step in both text and JSON — the first save —
/// instead of a null `recommended_action`.
#[test]
fn test_cli_init_empty_dir_recommends_first_save() {
    let temp = TempDir::new().unwrap();
    let text = heddle(&["init"], Some(temp.path())).unwrap();
    assert!(
        text.contains("heddle commit -m"),
        "text init output should point at the first save: {text}"
    );

    let temp = TempDir::new().unwrap();
    let json = heddle(&["--output", "json", "init"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        parsed["recommended_action"], "heddle commit -m \"...\"",
        "JSON init output carries the first-save recommendation: {parsed}"
    );
    assert_eq!(parsed["next_action"], parsed["recommended_action"]);
}

/// A non-quickstart init over existing Git history creates only the
/// Heddle sidecar. Git commits stay in `.git`, so the next save path is
/// the normal commit/checkpoint flow rather than adopt/import.
#[test]
fn test_cli_init_with_git_history_recommends_commit() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("seed.txt"), "history").unwrap();
    git_commit_all(temp.path(), "seed commit");

    let json = heddle(&["--output", "json", "init"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        parsed["recommended_action"], "heddle commit -m \"...\"",
        "init over existing Git history should point at the next checkpoint boundary: {parsed}"
    );
}

#[test]
fn test_cli_status_probes_plain_git_repo_without_initializing() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("plain.txt"), "drop-in status").unwrap();

    let status = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&status).unwrap();
    assert_eq!(parsed["repository_capability"], "plain-git");
    assert_eq!(parsed["storage_model"], "git-only");
    assert_eq!(parsed["git_branch"], "feature/drop-in");
    assert_eq!(parsed["heddle_initialized"], false);
    assert!(
        parsed["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "plain.txt"),
        "expected plain.txt in added paths: {parsed}"
    );
    assert!(!temp.path().join(".heddle").exists());
}

#[test]
fn test_cli_status_after_git_overlay_init_uses_git_backed_refs() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");

    heddle(&["init"], Some(temp.path())).unwrap();

    let status = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&status).unwrap();
    assert_eq!(parsed["repository_capability"], "git-overlay");
    assert_eq!(parsed["verification"]["status"], "clean");
    assert_eq!(parsed["verification"]["mapping_state"], "git_backed");
    assert!(parsed["recommended_action"].is_null(), "{parsed}");
    assert!(parsed["git_overlay_import_hint"].is_null(), "{parsed}");
    assert_eq!(parsed["changed_path_count"], 0);
    assert_eq!(parsed["thread_health"], "clean");
    assert!(
        parsed["attach_reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("Git-backed branch tip")),
        "attach reason should describe direct Git-backed storage: {parsed}"
    );
}

#[test]
fn test_cli_status_after_manual_git_commit_keeps_direct_git_backed_ref_clean() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle(&["init"], Some(temp.path())).unwrap();

    std::fs::write(temp.path().join("tracked.txt"), "captured by heddle\n").unwrap();
    heddle(
        &["capture", "-m", "capture before git commit"],
        Some(temp.path()),
    )
    .unwrap();

    std::fs::write(temp.path().join("tracked.txt"), "committed by git\n").unwrap();
    git_commit_all(temp.path(), "manual git commit");

    let status = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&status).unwrap();
    assert_eq!(parsed["repository_capability"], "git-overlay");
    assert_eq!(parsed["verification"]["status"], "clean");
    assert_eq!(parsed["verification"]["mapping_state"], "git_backed");
    assert!(parsed["recommended_action"].is_null(), "{parsed}");
    assert_eq!(parsed["changed_path_count"], 0, "{parsed}");
    assert_eq!(parsed["worktree_changed_path_count"], 0, "{parsed}");
    assert_eq!(parsed["thread_health"], "clean", "{parsed}");

    let verify = heddle(&["verify", "--output", "json"], Some(temp.path())).unwrap();
    let parsed_verify: Value = serde_json::from_str(&verify).unwrap();
    assert_eq!(parsed_verify["status"], "clean", "{parsed_verify}");
    assert_eq!(
        parsed_verify["mapping_state"], "git_backed",
        "{parsed_verify}"
    );
    assert_eq!(parsed_verify["worktree_state"], "clean", "{parsed_verify}");
}

#[test]
fn test_cli_discuss_open_bootstraps_clean_git_overlay_anchor() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle(&["init"], Some(temp.path())).unwrap();

    let list = heddle_output(&["--output", "json", "discuss", "list"], Some(temp.path()))
        .expect("invoke no-anchor discussion list");
    assert!(
        !list.status.success(),
        "read-only discussion list should refuse without creating a hidden anchor"
    );
    let list_stderr = std::str::from_utf8(&list.stderr).unwrap_or("");
    let advice: Value =
        serde_json::from_str(list_stderr).expect("discussion list refusal should be JSON advice");
    assert_eq!(advice["kind"], "repository_no_head");
    assert_eq!(advice["primary_command"], "heddle commit -m \"...\"");
    assert!(
        advice["recovery_commands"]
            .as_array()
            .is_some_and(|commands| commands
                .iter()
                .any(|command| { command.as_str() == Some("heddle checkpoint -m \"...\"") })),
        "no-anchor discussion advice should include a clean Git-overlay checkpoint path: {advice}"
    );

    let open = heddle_output(
        &[
            "--output",
            "json",
            "discuss",
            "open",
            "tracked.txt",
            "tracked",
            "anchor discussion",
        ],
        Some(temp.path()),
    )
    .expect("invoke discussion open");
    assert!(
        open.status.success(),
        "discussion open should bootstrap a clean Git-overlay anchor: stderr={}",
        std::str::from_utf8(&open.stderr).unwrap_or("")
    );
    let opened: Value =
        serde_json::from_slice(&open.stdout).expect("discussion open should emit JSON");
    assert_eq!(opened["output_kind"], "discuss_open");
    assert!(
        opened["opened_against_state"]
            .as_str()
            .is_some_and(|state| !state.is_empty()),
        "discussion should be anchored to the bootstrapped Heddle state: {opened}"
    );
}

#[test]
fn test_cli_color_force_emits_ansi_for_human_status() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());

    let output = heddle_output_with_env(
        &["--output", "text", "status"],
        Some(temp.path()),
        &[("CLICOLOR_FORCE", "1")],
    )
    .unwrap();
    assert!(output.status.success());
    let stdout = str::from_utf8(&output.stdout).unwrap_or("");
    assert!(
        stdout.contains("\x1b["),
        "forced color should preserve ANSI escapes in captured stdout: {stdout:?}"
    );
}

#[test]
fn test_cli_status_surfaces_git_import_hint_for_other_branches() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    let status = Command::new("git")
        .args(["branch", "support/import-me"])
        .current_dir(temp.path())
        .status()
        .expect("git branch should run");
    assert!(status.success());

    let output = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["recommended_action"], "heddle init");
    assert!(
        parsed["git_overlay_import_hint"].is_null(),
        "plain Git status should not surface an import hint under Git-backed overlay setup: {parsed}"
    );
    assert!(
        parsed["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .all(|value| value != "tracked.txt"),
        "tracked git baseline file should not appear dirty: {parsed}"
    );
}

#[test]
fn test_cli_status_in_plain_git_repo_distinguishes_modified_and_untracked() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    std::fs::write(temp.path().join("tracked.txt"), "tracked but modified").unwrap();
    std::fs::write(temp.path().join("plain.txt"), "new file").unwrap();

    let output = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();

    assert!(
        parsed["changes"]["modified"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "tracked.txt"),
        "tracked git file should show as modified: {parsed}"
    );
    assert!(
        parsed["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "plain.txt"),
        "new file should show as added: {parsed}"
    );
}

#[test]
fn test_cli_status_in_plain_git_repo_respects_gitignore() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join(".gitignore"), "ignored.log\n").unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    std::fs::write(temp.path().join("ignored.log"), "ignore me").unwrap();
    std::fs::write(temp.path().join("visible.txt"), "show me").unwrap();

    let output = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    let added = parsed["changes"]["added"].as_array().unwrap();

    assert!(
        added.iter().any(|value| value == "visible.txt"),
        "visible file should be present: {parsed}"
    );
    assert!(
        added.iter().all(|value| value != "ignored.log"),
        "ignored file should stay hidden: {parsed}"
    );
}

#[test]
fn test_cli_status_in_plain_git_repo_handles_detached_head() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    git(&["checkout", "--detach", "HEAD"], temp.path());
    std::fs::write(temp.path().join("plain.txt"), "detached work").unwrap();

    let output = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();

    assert!(
        parsed["thread"].is_null(),
        "detached HEAD should not fake a thread: {parsed}"
    );
    assert!(
        parsed["git_overlay_import_hint"].is_null(),
        "detached HEAD should not emit branch import hint: {parsed}"
    );
    assert!(
        parsed["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "plain.txt"),
        "detached worktree changes should still show up: {parsed}"
    );
}

#[test]
fn test_cli_status_surfaces_git_import_hint_for_many_branches() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    for branch in 0..12 {
        git(
            &["branch", &format!("support/import-{branch}")],
            temp.path(),
        );
    }

    // Import-hint information has moved to `heddle bridge git status
    // --output json`; per-command outputs no longer carry it.
    let output = heddle(
        &["bridge", "git", "status", "--output", "json"],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();

    assert_eq!(
        parsed["git_overlay_import_hint"]["missing_branch_count"],
        13
    );
    assert_eq!(
        parsed["git_overlay_import_hint"]["missing_branches"]
            .as_array()
            .unwrap()
            .len(),
        13
    );
    assert!(
        parsed["git_overlay_import_hint"]["missing_branches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|branch| branch.as_str() == Some("feature/drop-in")),
        "first-run bridge import hint should include the active branch: {parsed}"
    );
}

#[test]
fn test_cli_status_in_plain_git_repo_reports_staged_deletions() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    std::fs::remove_file(temp.path().join("tracked.txt")).unwrap();
    git(&["add", "-A"], temp.path());

    let output = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert!(
        parsed["changes"]["deleted"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "tracked.txt"),
        "staged deletion should show as deleted: {parsed}"
    );
}

#[test]
fn test_cli_status_in_plain_git_repo_works_from_subdirectory() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    let nested = temp.path().join("src/nested");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(temp.path().join("plain.txt"), "new file").unwrap();

    let output = heddle(&["status", "--output", "json"], Some(&nested)).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["git_branch"], "feature/drop-in");
    assert!(
        parsed["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "plain.txt"),
        "status from subdir should still see repo-root changes: {parsed}"
    );
}

#[test]
fn test_cli_diagnose_in_plain_git_repo_uses_git_baseline() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    std::fs::write(temp.path().join("tracked.txt"), "tracked but modified").unwrap();
    std::fs::write(temp.path().join("plain.txt"), "new file").unwrap();

    let output = heddle(&["doctor", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["repository_capability"], "plain-git");
    assert_eq!(parsed["git_overlay_health"]["status"], "needs_init");
    assert!(
        !temp.path().join(".heddle").exists(),
        "diagnose in a plain Git repo must be observe-only"
    );
    assert_eq!(parsed["changes"]["total"], 2);
    assert!(
        parsed["changes"]["modified"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "tracked.txt"),
        "diagnose should report tracked modification: {parsed}"
    );
    assert!(
        parsed["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "plain.txt"),
        "diagnose should report untracked addition: {parsed}"
    );
}

#[test]
fn test_cli_thread_list_in_plain_git_repo_respects_detached_head() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["checkout", "--detach", "HEAD"], temp.path());

    let output = heddle(&["thread", "list", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert!(
        parsed["current"].is_null(),
        "thread list should not claim a current branch in detached HEAD: {parsed}"
    );
}

#[test]
fn test_cli_workspace_in_plain_git_repo_respects_detached_head() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["checkout", "--detach", "HEAD"], temp.path());

    let output = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert!(
        parsed["thread"].is_null(),
        "status should not claim a current thread in detached HEAD: {parsed}"
    );
}

#[test]
fn test_cli_show_head_in_plain_git_repo_surfaces_import_hint() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/import-me"], temp.path());

    let output = heddle(&["show", "HEAD", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["repository_capability"], "plain-git");
    assert_eq!(parsed["recommended_action"], "heddle adopt");
    assert!(parsed["state"].is_null());
    assert!(
        !temp.path().join(".heddle").exists(),
        "show HEAD in a plain Git repo must be observe-only"
    );
}

#[test]
fn test_cli_log_in_plain_git_repo_surfaces_import_hint() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/import-me"], temp.path());

    let output = heddle(&["log", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["repository_capability"], "plain-git");
    assert_eq!(parsed["recommended_action"], "heddle adopt");
    assert!(parsed["states"].as_array().unwrap().is_empty());
    assert!(
        !temp.path().join(".heddle").exists(),
        "log in a plain Git repo must be observe-only"
    );
}

#[test]
fn test_cli_status_in_plain_git_repo_handles_mixed_staged_and_unstaged_changes() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    std::fs::write(temp.path().join("delete.txt"), "delete me").unwrap();
    git_commit_all(temp.path(), "seed branch");

    std::fs::write(temp.path().join("tracked.txt"), "tracked but modified").unwrap();
    std::fs::remove_file(temp.path().join("delete.txt")).unwrap();
    git(&["add", "delete.txt"], temp.path());
    std::fs::write(temp.path().join("plain.txt"), "new file").unwrap();

    let output = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert!(
        parsed["changes"]["modified"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "tracked.txt"),
        "modified tracked file missing: {parsed}"
    );
    assert!(
        parsed["changes"]["deleted"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "delete.txt"),
        "staged deletion missing: {parsed}"
    );
    assert!(
        parsed["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "plain.txt"),
        "untracked addition missing: {parsed}"
    );
}

#[test]
fn test_cli_status_in_plain_git_repo_handles_git_rename_as_delete_plus_add() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("old_name.txt"), "rename me").unwrap();
    git_commit_all(temp.path(), "seed branch");

    std::fs::rename(
        temp.path().join("old_name.txt"),
        temp.path().join("new_name.txt"),
    )
    .unwrap();
    git(&["add", "-A"], temp.path());

    let output = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert!(
        parsed["changes"]["deleted"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "old_name.txt"),
        "git rename should expose deleted old path: {parsed}"
    );
    assert!(
        parsed["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "new_name.txt"),
        "git rename should expose added new path: {parsed}"
    );
}

#[test]
fn test_cli_ready_in_plain_git_repo_captures_mixed_git_state() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle_adopt(temp.path());

    std::fs::write(temp.path().join("tracked.txt"), "tracked but modified").unwrap();
    std::fs::write(temp.path().join("plain.txt"), "new file").unwrap();

    let ready_output = heddle_output(
        &["--output", "json", "ready", "-m", "ready mixed git state"],
        Some(temp.path()),
    )
    .expect("invoke ready");
    assert!(
        !ready_output.status.success(),
        "ready should preserve the capture but require a Git checkpoint before claiming verification"
    );
    let ready = inject_post_verification_at(
        temp.path(),
        json_stdout(&ready_output, "ready blocked after capture"),
    );
    assert_eq!(ready["status"], "blocked");
    assert_eq!(ready["captured"], true);
    assert_eq!(ready["verification"]["status"], "needs_checkpoint");
    assert_eq!(ready["recommended_action"], "heddle commit -m \"...\"");

    let status: Value =
        serde_json::from_str(&heddle(&["status", "--output", "json"], Some(temp.path())).unwrap())
            .unwrap();
    assert!(status["state"]["change_id"].as_str().is_some());
    assert_eq!(status["verification"]["status"], "needs_checkpoint");
    assert_eq!(status["recommended_action"], "heddle commit -m \"...\"");
}

#[test]
fn test_cli_compare_in_plain_git_repo_bootstraps_from_git_overlay_head() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle_adopt(temp.path());

    std::fs::write(temp.path().join("tracked.txt"), "tracked but modified").unwrap();

    let output = heddle(
        &["--output", "json", "diff", "HEAD", "HEAD"],
        Some(temp.path()),
    )
    .unwrap();
    // `diff` (which absorbed `compare`) emits JSON; assert the schema is
    // present and resolved a state on both sides instead of grepping for
    // legacy human-text markers.
    let parsed: Value = serde_json::from_str(&output)
        .unwrap_or_else(|err| panic!("diff output should be JSON: {err}; raw: {output}"));
    assert!(
        parsed["from_state"].as_str().is_some(),
        "diff must resolve from_state: {output}"
    );
    assert!(
        parsed["to_state"].as_str().is_some(),
        "diff must resolve to_state: {output}"
    );
    assert!(
        parsed["stats"].is_object(),
        "diff must include a stats block: {output}"
    );
}

#[test]
fn test_cli_merge_preview_rejects_dirty_plain_git_repo_after_bootstrap() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle_adopt(temp.path());

    let started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "start",
                "feature/preview-thread",
                "--workspace",
                "auto",
            ],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let thread_path = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());
    std::fs::write(thread_path.join("thread.txt"), "thread work").unwrap();
    heddle(&["capture", "-m", "Thread capture"], Some(&thread_path)).unwrap();

    std::fs::write(temp.path().join("dirty.txt"), "dirty main worktree").unwrap();

    let err = heddle_output(
        &[
            "--output",
            "json",
            "merge",
            "feature/preview-thread",
            "--preview",
        ],
        Some(temp.path()),
    )
    .expect("invoke merge preview");
    assert!(
        !err.status.success(),
        "merge preview should reject dirty current worktree"
    );
    let envelope: Value = serde_json::from_slice(&err.stderr)
        .unwrap_or_else(|json_err| panic!("merge refusal should be JSON: {json_err}; {err:?}"));
    assert_eq!(envelope["kind"], "dirty_worktree");
    assert!(
        envelope["unsafe_condition"]
            .as_str()
            .is_some_and(|condition| condition.contains("dirty.txt")),
        "merge preview should list dirty paths: {envelope}"
    );
    assert_eq!(
        envelope["recovery_commands"],
        serde_json::json!([
            "heddle commit -m \"...\"",
            "heddle capture -m \"...\"",
            "heddle stash push -m \"...\""
        ]),
        "merge preview should keep shared preservation commands: {envelope}"
    );
}

#[test]
fn test_cli_compare_head_head_bootstraps_in_plain_git_repo() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle_adopt(temp.path());

    let output = heddle(
        &["--output", "json", "diff", "HEAD", "HEAD"],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(
        parsed["stats"]["files_changed"], 0,
        "diff HEAD HEAD should succeed and be empty: {parsed}"
    );
}

#[test]
fn test_cli_diff_head_to_worktree_in_plain_git_repo_uses_git_overlay_baseline() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    std::fs::write(temp.path().join("tracked.txt"), "tracked but modified").unwrap();

    let output = heddle(&["--output", "json", "diff", "HEAD"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert!(
        !temp.path().join(".heddle").exists(),
        "diff HEAD in a plain Git repo must be observe-only"
    );
    assert!(
        parsed["changes"]["modified"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| change["path"] == "tracked.txt"),
        "diff from HEAD should reflect tracked modification under the modified category: {parsed}"
    );
}

#[test]
fn test_cli_status_in_plain_git_repo_handles_deeper_history_and_many_branches() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    seed_git_history(temp.path(), 8);

    for branch in 0..20 {
        git(
            &["branch", &format!("support/history-{branch}")],
            temp.path(),
        );
    }
    std::fs::write(temp.path().join("plain.txt"), "new file").unwrap();

    let output = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["git_branch"], "feature/drop-in");
    assert!(
        parsed["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "plain.txt"),
        "plain file should remain visible in larger git fixture: {parsed}"
    );

    // Import-hint information has moved to `heddle bridge git status
    // --output json`; per-command outputs no longer carry it.
    let bridge_output = heddle(
        &["bridge", "git", "status", "--output", "json"],
        Some(temp.path()),
    )
    .unwrap();
    let bridge: Value = serde_json::from_str(&bridge_output).unwrap();
    assert_eq!(
        bridge["git_overlay_import_hint"]["missing_branch_count"],
        21
    );
    assert!(
        bridge["git_overlay_import_hint"]["missing_branches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|branch| branch.as_str() == Some("feature/drop-in")),
        "first-run bridge import hint should include the active branch: {bridge}"
    );
}

#[test]
fn test_cli_log_in_plain_git_repo_handles_deeper_history_and_many_branches() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    seed_git_history(temp.path(), 6);

    for branch in 0..10 {
        git(&["branch", &format!("support/log-{branch}")], temp.path());
    }

    let output = heddle(&["log", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["repository_capability"], "plain-git");
    assert!(parsed["states"].as_array().unwrap().is_empty());
    assert!(
        !temp.path().join(".heddle").exists(),
        "log in a deeper plain Git fixture must be observe-only"
    );

    heddle(&["init"], Some(temp.path())).unwrap();
    let bridge_output = heddle(
        &["bridge", "git", "status", "--output", "json"],
        Some(temp.path()),
    )
    .unwrap();
    let bridge: Value = serde_json::from_str(&bridge_output).unwrap();
    assert_eq!(
        bridge["git_overlay_import_hint"]["missing_branch_count"],
        11
    );
}

#[test]
fn test_cli_status_tracks_git_branch_switch_after_bootstrap() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/switch-me"], temp.path());

    let first: Value =
        serde_json::from_str(&heddle(&["status", "--output", "json"], Some(temp.path())).unwrap())
            .unwrap();
    assert_eq!(first["git_branch"], "feature/drop-in");

    git(&["checkout", "support/switch-me"], temp.path());
    std::fs::write(temp.path().join("switch.txt"), "switched").unwrap();

    let second: Value =
        serde_json::from_str(&heddle(&["status", "--output", "json"], Some(temp.path())).unwrap())
            .unwrap();
    assert_eq!(second["git_branch"], "support/switch-me");
    assert!(
        second["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "switch.txt"),
        "dirty files should still be reported after branch switch: {second}"
    );

    // Import-hint information has moved to `heddle bridge git status
    // --output json`; per-command outputs no longer carry it.
    let bridge_output = heddle(
        &["bridge", "git", "status", "--output", "json"],
        Some(temp.path()),
    )
    .unwrap();
    let bridge: Value = serde_json::from_str(&bridge_output).unwrap();
    assert!(
        bridge["git_overlay_import_hint"]["missing_branches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "feature/drop-in"),
        "after switching branches, the old branch should become importable history: {bridge}"
    );
}

#[test]
fn test_cli_workspace_tracks_git_branch_switch_after_bootstrap() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/workspace-switch"], temp.path());

    let _ = heddle(&["init"], Some(temp.path())).unwrap();
    git(&["checkout", "support/workspace-switch"], temp.path());

    let output = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["thread"], "support/workspace-switch");
}

#[test]
fn test_cli_thread_list_tracks_git_branch_switch_after_bootstrap() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/thread-switch"], temp.path());

    let _ = heddle(&["init"], Some(temp.path())).unwrap();
    git(&["checkout", "support/thread-switch"], temp.path());

    let output = heddle(&["thread", "list", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["current"], "support/thread-switch");
}

#[test]
fn test_cli_status_handles_detached_head_after_bootstrap() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    let _ = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    git(&["checkout", "--detach", "HEAD"], temp.path());

    let output = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert!(
        parsed["thread"].is_null(),
        "detached HEAD should clear current thread: {parsed}"
    );
    assert!(
        parsed["git_overlay_import_hint"].is_null(),
        "detached HEAD should clear import hint after bootstrap too: {parsed}"
    );
}

#[test]
fn test_cli_bridge_git_import_clears_import_hint_for_existing_branches() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/import-me"], temp.path());

    let before: Value = serde_json::from_str(
        &heddle(
            &["bridge", "git", "status", "--output", "json"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(before["git_overlay_import_hint"]["missing_branch_count"], 2);

    let import_output = heddle(&["bridge", "import", "--path", "."], Some(temp.path())).unwrap();
    let parsed_import: serde_json::Value =
        serde_json::from_str(&import_output).unwrap_or(serde_json::Value::Null);
    let synced = parsed_import["branches_synced"].as_u64().unwrap_or(0);
    assert!(
        synced >= 1 || import_output.contains("Synced") || import_output.contains("branches"),
        "bridge import should sync local branches: {import_output}"
    );

    let after: Value = serde_json::from_str(
        &heddle(
            &["bridge", "git", "status", "--output", "json"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(
        after["git_overlay_import_hint"].is_null(),
        "importing Git branches should clear the import hint: {after}"
    );

    let threads: Value = serde_json::from_str(
        &heddle(&["thread", "list", "--output", "json"], Some(temp.path())).unwrap(),
    )
    .unwrap();
    assert!(
        threads["threads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|thread| thread["name"] == "support/import-me"),
        "thread list should include imported Git branch: {threads}"
    );
}

#[test]
fn test_cli_bridge_git_import_ref_imports_only_selected_branch() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/import-me"], temp.path());
    git(&["branch", "support/leave-alone"], temp.path());

    let import_output = heddle(
        &[
            "--output",
            "json",
            "bridge",
            "git",
            "import",
            "--path",
            ".",
            "--ref",
            "support/import-me",
        ],
        Some(temp.path()),
    )
    .unwrap();
    let parsed_import: Value = serde_json::from_str(&import_output).unwrap_or(Value::Null);
    assert!(
        parsed_import["branches_synced"].as_u64() == Some(1)
            || import_output.contains("Synced 1 branches to threads"),
        "ref-scoped import should sync only one branch: {import_output}"
    );

    let threads: Value = serde_json::from_str(
        &heddle(&["thread", "list", "--output", "json"], Some(temp.path())).unwrap(),
    )
    .unwrap();
    assert!(
        threads["threads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|thread| thread["name"] == "support/import-me"
                && thread["history_imported"] == true),
        "selected branch should be imported: {threads}"
    );
    assert!(
        threads["available_git_refs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|thread| thread["name"] == "support/leave-alone"),
        "unselected branch should remain available as a Git-only ref: {threads}"
    );
}

#[test]
fn test_cli_show_git_only_branch_tip_suggests_ref_scoped_import() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/git-only"], temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle(
        &["show", "support/git-only", "--output", "json"],
        Some(temp.path()),
    )
    .unwrap_err()
    .to_string();
    assert!(
        output.contains("heddle adopt --ref support/git-only"),
        "show should recommend a ref-scoped import for git-only branch tips: {output}"
    );
}

#[test]
fn test_cli_show_git_only_tag_suggests_ref_scoped_import() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["tag", "v1.0.0"], temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle(&["show", "v1.0.0", "--output", "json"], Some(temp.path()))
        .unwrap_err()
        .to_string();
    assert!(
        output.contains("heddle adopt --ref v1.0.0"),
        "show should recommend a ref-scoped import for git-only tags: {output}"
    );
}

#[test]
fn test_cli_diff_mapped_git_branch_alias_resolves_without_import_loop() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle_adopt(temp.path());
    git(&["branch", "support/git-only"], temp.path());

    let output = heddle(
        &["diff", "HEAD", "support/git-only", "--output", "json"],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["status"], "completed");
    assert_eq!(
        parsed["changes"]
            .as_array()
            .expect("changes should be an array")
            .len(),
        0,
        "branch aliases at already-mapped Git commits should resolve without an import loop: {parsed}"
    );
}

#[test]
fn test_cli_compare_mapped_git_tag_resolves_without_import_loop() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle_adopt(temp.path());
    git(&["tag", "v1.0.0"], temp.path());

    let output = heddle(
        &["diff", "HEAD", "v1.0.0", "--output", "json"],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["stats"]["files_changed"], 0);
    assert_eq!(
        parsed["changes"]
            .as_array()
            .expect("changes should be an array")
            .len(),
        0,
        "tags at already-mapped Git commits should resolve without an import loop: {parsed}"
    );
}

#[test]
fn test_cli_thread_list_marks_tip_only_branch_with_ref_scoped_import_action() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/git-only"], temp.path());
    let _ = heddle(&["init"], Some(temp.path())).unwrap();

    let threads: Value = serde_json::from_str(
        &heddle(&["thread", "list", "--output", "json"], Some(temp.path())).unwrap(),
    )
    .unwrap();
    assert!(
        threads["threads"]
            .as_array()
            .unwrap()
            .iter()
            .all(|thread| thread["name"] != "support/git-only"),
        "Git-only refs should not be shaped as active threads: {threads}"
    );
    let available_ref = threads["available_git_refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|git_ref| git_ref["name"] == "support/git-only")
        .expect("support/git-only should be visible as an available Git ref");
    assert!(
        available_ref["git_commit"]
            .as_str()
            .is_some_and(|oid| !oid.is_empty()),
        "available Git refs should expose their Git tip: {threads}"
    );
    assert_eq!(
        available_ref["recommended_action"],
        "heddle adopt --ref support/git-only"
    );
}

#[test]
fn test_cli_bridge_git_import_ref_imports_only_selected_tag() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["tag", "v1.0.0"], temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked next").unwrap();
    git_commit_all(temp.path(), "second commit");
    git(&["tag", "v2.0.0"], temp.path());

    let import_output = heddle(
        &[
            "--output", "json", "bridge", "git", "import", "--path", ".", "--ref", "v1.0.0",
        ],
        Some(temp.path()),
    )
    .unwrap();
    let parsed_import: Value = serde_json::from_str(&import_output).unwrap_or(Value::Null);
    assert!(
        parsed_import["tags_synced"].as_u64() == Some(1)
            || import_output.contains("Synced 1 tags to markers"),
        "expected selected tag import output: {import_output}"
    );

    let v1 = heddle(&["show", "v1.0.0", "--output", "json"], Some(temp.path())).unwrap();
    let parsed_v1: Value = serde_json::from_str(&v1).unwrap();
    assert!(parsed_v1["change_id"].as_str().is_some());

    let v2_err = heddle(&["show", "v2.0.0", "--output", "json"], Some(temp.path()))
        .unwrap_err()
        .to_string();
    assert!(
        v2_err.contains("heddle adopt --ref v2.0.0"),
        "unselected tag should remain import-only: {v2_err}"
    );
}

#[test]
fn test_cli_bridge_git_import_defaults_to_current_repo_even_after_mirror_exists() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    heddle(&["bridge", "import", "--path", "."], Some(temp.path())).unwrap();

    git(&["branch", "support/import-latest"], temp.path());

    let import_output = heddle(&["bridge", "import"], Some(temp.path())).unwrap();
    let parsed_import: Value = serde_json::from_str(&import_output).unwrap_or(Value::Null);
    let synced = parsed_import["branches_synced"].as_u64().unwrap_or(0);
    assert!(
        synced >= 1 || import_output.contains("Synced") || import_output.contains("branches"),
        "expected live current repo import, not stale mirror import: {import_output}"
    );

    let threads: Value = serde_json::from_str(
        &heddle(&["thread", "list", "--output", "json"], Some(temp.path())).unwrap(),
    )
    .unwrap();
    assert!(
        threads["threads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|thread| thread["name"] == "support/import-latest"
                && thread["history_imported"] == true),
        "default import should read the current repo and pick up the latest branch: {threads}"
    );
}

#[test]
fn test_cli_diagnose_tracks_git_branch_switch_after_bootstrap() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/diagnose-switch"], temp.path());

    let _ = heddle(&["doctor", "--output", "json"], Some(temp.path())).unwrap();
    git(&["checkout", "support/diagnose-switch"], temp.path());
    std::fs::write(temp.path().join("diag.txt"), "dirty").unwrap();

    let output = heddle(&["doctor", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["repository_capability"], "plain-git");
    assert_eq!(parsed["git_overlay_health"]["status"], "needs_init");
    assert_eq!(
        parsed["git_overlay_import_hint"]["missing_branches"][0],
        "support/diagnose-switch"
    );
    assert!(
        !temp.path().join(".heddle").exists(),
        "diagnose should not bootstrap plain Git before explicit init"
    );
    assert!(
        parsed["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "diag.txt"),
        "diagnose should still reflect dirty state after branch switch: {parsed}"
    );
}

#[test]
fn test_cli_show_head_tracks_git_branch_switch_after_bootstrap() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/show-switch"], temp.path());
    git(&["checkout", "support/show-switch"], temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "support tracked").unwrap();
    git_commit_all(temp.path(), "support branch");
    git(&["checkout", "feature/drop-in"], temp.path());

    heddle_adopt(temp.path());
    let before: Value = serde_json::from_str(
        &heddle(&["show", "HEAD", "--output", "json"], Some(temp.path())).unwrap(),
    )
    .unwrap();
    git(&["checkout", "support/show-switch"], temp.path());

    let output = heddle(&["show", "HEAD", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["repository_capability"], "git-overlay");
    assert!(parsed["change_id"].as_str().is_some());
    assert_ne!(
        parsed["change_id"], before["change_id"],
        "show HEAD should follow the switched Git branch, not stale bootstrap state: {parsed}"
    );
}

#[test]
fn test_cli_ready_captures_current_git_branch_after_switch() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/ready-switch"], temp.path());

    heddle_adopt(temp.path());
    git(&["checkout", "support/ready-switch"], temp.path());
    std::fs::write(temp.path().join("ready.txt"), "capture me").unwrap();

    let ready_output = heddle_output(
        &["--output", "json", "ready", "-m", "ready switched branch"],
        Some(temp.path()),
    )
    .expect("invoke ready");
    assert!(
        !ready_output.status.success(),
        "ready should preserve the capture but require a Git checkpoint before claiming verification"
    );
    let ready = inject_post_verification_at(
        temp.path(),
        json_stdout(&ready_output, "ready blocked after switched branch capture"),
    );
    assert_eq!(ready["status"], "blocked");
    assert_eq!(ready["captured"], true);
    assert_eq!(ready["verification"]["status"], "needs_checkpoint");
    assert_eq!(ready["recommended_action"], "heddle commit -m \"...\"");

    let status: Value =
        serde_json::from_str(&heddle(&["status", "--output", "json"], Some(temp.path())).unwrap())
            .unwrap();
    assert_eq!(status["thread"], "support/ready-switch");
    assert!(status["state"]["change_id"].as_str().is_some());
    assert_eq!(status["verification"]["status"], "needs_checkpoint");
    assert_eq!(status["recommended_action"], "heddle commit -m \"...\"");
}

#[test]
fn test_cli_workspace_surfaces_git_import_hint_in_text_output() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    let status = Command::new("git")
        .args(["branch", "support/import-me"])
        .current_dir(temp.path())
        .status()
        .expect("git branch should run");
    assert!(status.success());
    let _ = heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle(&["thread", "list"], Some(temp.path())).unwrap();
    assert!(
        output.contains("support/import-me"),
        "missing branch hint: {output}"
    );
    assert!(
        output.contains("heddle adopt"),
        "missing import command: {output}"
    );
}

#[test]
fn test_cli_init_with_principal() {
    let temp = TempDir::new().unwrap();
    let config_path = temp.path().join("heddle-user.toml");

    let result = heddle_output_with_env(
        &[
            "init",
            "--principal-name",
            "Test User",
            "--principal-email",
            "test@example.com",
        ],
        Some(temp.path()),
        &[("HEDDLE_CONFIG", config_path.to_str().unwrap())],
    );
    assert!(result.is_ok());

    let config = UserConfig::load(&config_path).unwrap();
    let principal = config.principal.expect("principal should be set");
    assert_eq!(principal.name, "Test User");
    assert_eq!(principal.email, "test@example.com");
}

#[test]
fn test_cli_status_on_empty_repo() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle(&["status"], Some(temp.path())).unwrap();
    assert!(
        output.contains("On thread: main") || output.contains("main"),
        "Should show current thread"
    );
}

#[test]
fn test_cli_status_shows_untracked_files() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("test.txt"), "hello").unwrap();

    let output = heddle(&["status"], Some(temp.path())).unwrap();
    assert!(
        output.contains("test.txt") || output.contains("added") || output.contains("untracked"),
        "Should show untracked file: {}",
        output
    );
}

#[test]
fn test_cli_snapshot_creates_state() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("hello.txt"), "world").unwrap();

    let output = heddle(&["capture", "-m", "Initial commit"], Some(temp.path())).unwrap();
    assert!(
        output.contains("Created state") || output.contains("hd-"),
        "Should show created state: {}",
        output
    );
}

/// Concurrent writers on the same checkout must not race the Git
/// index. We assert that:
///
///   1. A leftover `index.lock` causes `heddle checkpoint` to bail
///      with the structured `IndexAlreadyDirty` skip reason instead
///      of clobbering the index.
///   2. Once the lock is removed, the next checkpoint succeeds and
///      cleans up its own lock (no stale `.git/index.lock` remains).
#[test]
fn test_cli_checkpoint_skips_when_git_index_is_locked() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("hello.txt"), "world").unwrap();
    heddle(&["capture", "-m", "initial"], Some(temp.path())).unwrap();

    // Simulate another writer holding the canonical Git index lock.
    let lock_path = temp.path().join(".git").join("index.lock");
    std::fs::write(&lock_path, b"").unwrap();

    let blocked = heddle_output(
        &["--output", "json", "checkpoint", "-m", "blocked checkpoint"],
        Some(temp.path()),
    )
    .expect("invoke locked checkpoint");
    assert!(
        !blocked.status.success(),
        "checkpoint must refuse to write through a locked index"
    );
    assert!(
        blocked.stdout.is_empty(),
        "JSON-mode checkpoint refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&blocked.stdout)
    );
    let stderr = std::str::from_utf8(&blocked.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("locked checkpoint should emit JSON envelope");
    assert_eq!(envelope["kind"], "checkpoint_git_write_skipped");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("locked") || error.contains("index")),
        "checkpoint must explain the index-lock conflict with typed advice: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle checkpoint -m")),
        "checkpoint hint should name the retry command: {stderr}"
    );
    assert!(
        lock_path.exists(),
        "checkpoint must not delete an externally-held index.lock"
    );

    // Drop the foreign lock and retry. Subsequent checkpoint should
    // succeed and tidy its own index.lock so the directory is left
    // clean.
    std::fs::remove_file(&lock_path).unwrap();
    heddle(
        &["checkpoint", "-m", "post-unlock checkpoint"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        !lock_path.exists(),
        "successful checkpoint must release its index.lock; found leftover at {}",
        lock_path.display()
    );
}

#[test]
fn test_cli_checkpoint_creates_git_commit_and_records_mapping() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("hello.txt"), "world").unwrap();

    heddle(
        &["capture", "-m", "Initial overlay capture"],
        Some(temp.path()),
    )
    .unwrap();
    let output = heddle(
        &[
            "--output",
            "json",
            "checkpoint",
            "-m",
            "Initial Git checkpoint",
        ],
        Some(temp.path()),
    )
    .unwrap();
    let checkpoint: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(checkpoint["summary"], "Initial Git checkpoint");

    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(temp.path())
        .output()
        .expect("git rev-parse should run");
    assert!(head.status.success());
    let git_commit = String::from_utf8(head.stdout).unwrap().trim().to_string();
    assert!(!git_commit.is_empty());

    // Heddle records change_id provenance on `refs/notes/heddle`
    // inside the bridge mirror at `.heddle/git/` rather than rewriting
    // commit messages with a `Heddle-Change:` trailer — that keeps Git
    // commit SHAs stable across heddle imports/exports and keeps normal
    // `git log --all` in the user's repo free of Heddle metadata roots.
    let notes = Command::new("git")
        .arg(format!(
            "--git-dir={}",
            temp.path().join(".heddle/git").display()
        ))
        .args(["notes", "--ref=refs/notes/heddle", "show", &git_commit])
        .current_dir(temp.path())
        .output()
        .expect("git notes show should run");
    assert!(
        notes.status.success(),
        "expected refs/notes/heddle in the bridge mirror to record the checkpoint commit; stderr: {}",
        String::from_utf8_lossy(&notes.stderr)
    );
    let note_body = String::from_utf8(notes.stdout).unwrap();
    assert!(
        note_body.contains("hd-"),
        "note body should embed a Heddle change id: {note_body}"
    );
    let git_log = Command::new("git")
        .args(["log", "--all", "--format=%s"])
        .current_dir(temp.path())
        .output()
        .expect("git log --all should run");
    assert!(git_log.status.success());
    let git_log = String::from_utf8(git_log.stdout).unwrap();
    assert!(
        !git_log.contains("heddle: state metadata"),
        "user Git history should not expose Heddle metadata commits: {git_log}"
    );

    let status = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&status).unwrap();
    assert_eq!(parsed["git_checkpoint"]["git_commit"], git_commit);
}

#[test]
fn test_cli_checkpoint_refuses_plain_git_repo_before_adoption() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("checkpoint.txt"), "checkpoint me").unwrap();

    let output = heddle_output(
        &[
            "--output",
            "json",
            "checkpoint",
            "-m",
            "Bootstrap Git checkpoint",
        ],
        Some(temp.path()),
    )
    .expect("invoke checkpoint before adoption");
    assert!(
        !output.status.success(),
        "checkpoint should refuse plain Git instead of implicitly adopting"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let envelope: Value =
        serde_json::from_slice(&output.stderr).expect("refusal should be JSON advice");
    assert_eq!(envelope["kind"], "git_repo_needs_adoption");
    assert_eq!(envelope["primary_command"], "heddle init");

    assert!(
        !temp.path().join(".heddle").exists(),
        "refused checkpoint must not create Heddle metadata"
    );
}

#[test]
fn test_cli_ready_in_git_overlay_auto_captures_initial_state() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("ready.txt"), "capture me").unwrap();

    let ready_output = heddle_output(
        &["--output", "json", "ready", "-m", "ready initial state"],
        Some(temp.path()),
    )
    .expect("invoke ready");
    assert!(
        !ready_output.status.success(),
        "ready should preserve the capture but require a Git checkpoint before claiming verification"
    );
    let ready = inject_post_verification_at(
        temp.path(),
        json_stdout(&ready_output, "ready blocked after initial capture"),
    );
    assert_eq!(ready["status"], "blocked");
    assert_eq!(ready["captured"], true);
    assert_eq!(ready["verification"]["status"], "needs_checkpoint");
    assert_eq!(ready["recommended_action"], "heddle commit -m \"...\"");

    let status: Value =
        serde_json::from_str(&heddle(&["status", "--output", "json"], Some(temp.path())).unwrap())
            .unwrap();
    assert!(status["state"]["change_id"].as_str().is_some());
    assert!(status["git_checkpoint"].is_null());
    assert_eq!(status["verification"]["status"], "needs_checkpoint");
    assert_eq!(status["recommended_action"], "heddle commit -m \"...\"");
}

#[test]
fn test_cli_start_bootstraps_current_state_in_plain_git_repo() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("start.txt"), "start from git").unwrap();

    let started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "start",
                "feature/overlay-thread",
                "--workspace",
                "auto",
            ],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(started["name"], "feature/overlay-thread");
    assert!(started["execution_path"].as_str().is_some());

    let status: Value =
        serde_json::from_str(&heddle(&["status", "--output", "json"], Some(temp.path())).unwrap())
            .unwrap();
    assert!(status["state"]["change_id"].as_str().is_some());
}

#[test]
fn test_cli_marker_create_bootstraps_current_state_in_plain_git_repo() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("marker.txt"), "mark me").unwrap();

    let output = heddle(
        &["thread", "marker", "create", "bootstrap-marker"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(output.contains("bootstrap-marker"));

    let status: Value =
        serde_json::from_str(&heddle(&["status", "--output", "json"], Some(temp.path())).unwrap())
            .unwrap();
    assert!(status["state"]["change_id"].as_str().is_some());
}

#[test]
fn test_cli_thread_create_bootstraps_current_state_in_plain_git_repo() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("thread.txt"), "thread me").unwrap();

    let output = heddle(
        &["thread", "create", "feature/create-thread"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(output.contains("feature/create-thread"));

    let status: Value =
        serde_json::from_str(&heddle(&["status", "--output", "json"], Some(temp.path())).unwrap())
            .unwrap();
    assert!(status["state"]["change_id"].as_str().is_some());
}

#[test]
fn test_cli_show_head_guides_unborn_plain_git_repo_without_bootstrap() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("show.txt"), "show me").unwrap();

    let output = heddle(&["--output", "json", "show", "HEAD"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["repository_capability"], "plain-git");
    assert_eq!(parsed["recommended_action"], "heddle init");
    assert!(
        !temp.path().join(".heddle").exists(),
        "show HEAD in a plain Git repo must not bootstrap"
    );
}

#[test]
fn test_cli_log_guides_unborn_plain_git_repo_without_bootstrap() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("log.txt"), "log me").unwrap();

    let output = heddle(&["log", "--oneline"], Some(temp.path())).unwrap();
    assert!(output.contains("heddle init"));
    assert!(
        !temp.path().join(".heddle").exists(),
        "log in a plain Git repo must not bootstrap"
    );
}

#[test]
fn test_cli_ship_in_git_overlay_auto_checkpoints() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle_adopt(temp.path());

    let started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "start",
                "feature/land-it",
                "--workspace",
                "auto",
            ],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let thread = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());
    std::fs::write(thread.join("land.txt"), "land me").unwrap();

    let landed: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "land", "--thread", "feature/land-it"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(landed["status"], "landed");
    assert_eq!(landed["checkpointed"], true);
    assert!(landed["git_commit"].as_str().is_some());
    assert!(temp.path().join("land.txt").exists());

    let status: Value =
        serde_json::from_str(&heddle(&["status", "--output", "json"], Some(temp.path())).unwrap())
            .unwrap();
    assert!(status["git_checkpoint"]["git_commit"].as_str().is_some());
}

#[test]
fn test_parallel_heddle_threads_capture_independently_and_checkpoint_via_git_overlay_root() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle_adopt(temp.path());

    let auth_started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "start",
                "feature/auth",
                "--workspace",
                "auto",
            ],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let search_started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "start",
                "feature/search",
                "--workspace",
                "auto",
            ],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();

    let auth_path = std::path::PathBuf::from(auth_started["execution_path"].as_str().unwrap());
    let search_path = std::path::PathBuf::from(search_started["execution_path"].as_str().unwrap());

    std::fs::write(auth_path.join("auth.rs"), "auth v1").unwrap();
    heddle(&["capture", "-m", "auth v1"], Some(&auth_path)).unwrap();
    std::fs::write(auth_path.join("auth.rs"), "auth v2").unwrap();
    let auth_capture: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "capture", "-m", "auth v2"],
            Some(&auth_path),
        )
        .unwrap(),
    )
    .unwrap();

    std::fs::write(search_path.join("search.rs"), "search v1").unwrap();
    heddle(&["capture", "-m", "search v1"], Some(&search_path)).unwrap();
    std::fs::write(search_path.join("search.rs"), "search v2").unwrap();
    let search_capture: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "capture", "-m", "search v2"],
            Some(&search_path),
        )
        .unwrap(),
    )
    .unwrap();

    let auth_thread: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "thread", "show", "feature/auth"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let search_thread: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "thread", "show", "feature/search"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        auth_thread["current_state"].as_str().unwrap(),
        auth_capture["change_id"].as_str().unwrap()
    );
    assert_eq!(
        search_thread["current_state"].as_str().unwrap(),
        search_capture["change_id"].as_str().unwrap()
    );

    let auth_checkpoint_err = heddle(
        &["checkpoint", "-m", "auth direct checkpoint"],
        Some(&auth_path),
    )
    .unwrap_err();
    assert!(
        auth_checkpoint_err.contains("only for Git-overlay repositories"),
        "isolated auth thread should reject direct checkpoint: {auth_checkpoint_err}"
    );
    let search_checkpoint_err = heddle(
        &["checkpoint", "-m", "search direct checkpoint"],
        Some(&search_path),
    )
    .unwrap_err();
    assert!(
        search_checkpoint_err.contains("only for Git-overlay repositories"),
        "isolated search thread should reject direct checkpoint: {search_checkpoint_err}"
    );

    let auth_ship: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "land", "--thread", "feature/auth"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(auth_ship["status"], "landed");
    assert_eq!(auth_ship["checkpointed"], true);
    assert!(auth_ship["git_commit"].as_str().is_some());

    let search_refresh: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "thread", "refresh", "feature/search"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(search_refresh["status"], "completed");

    let search_ship: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "land", "--thread", "feature/search"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(search_ship["status"], "landed");
    assert_eq!(search_ship["checkpointed"], true);
    assert!(search_ship["git_commit"].as_str().is_some());

    assert!(temp.path().join("auth.rs").exists());
    assert!(temp.path().join("search.rs").exists());

    let checkpoint_records_path = temp.path().join(".heddle/state/git-checkpoints.json");
    let checkpoint_records: Value =
        serde_json::from_str(&std::fs::read_to_string(checkpoint_records_path).unwrap()).unwrap();
    let records = checkpoint_records.as_array().unwrap();
    assert!(
        records.len() >= 2,
        "expected at least two git checkpoint records after shipping both threads: {checkpoint_records}"
    );
    assert!(
        records
            .iter()
            .any(|record| record["summary"] == "Land feature/auth"),
        "shipping auth should create its own git checkpoint record: {checkpoint_records}"
    );
    assert!(
        records
            .iter()
            .any(|record| record["summary"] == "Land feature/search"),
        "shipping search should create its own git checkpoint record: {checkpoint_records}"
    );
    assert_ne!(
        auth_ship["git_commit"], search_ship["git_commit"],
        "separate landed threads should produce distinct git commits"
    );

    // Each landed thread should record a Heddle change id on the
    // bridge mirror's `refs/notes/heddle` ref without publishing the
    // metadata notes ref into the user's ordinary `.git/refs`.
    for git_commit in [
        auth_ship["git_commit"].as_str().unwrap(),
        search_ship["git_commit"].as_str().unwrap(),
    ] {
        let notes = Command::new("git")
            .arg(format!(
                "--git-dir={}",
                temp.path().join(".heddle/git").display()
            ))
            .args(["notes", "--ref=refs/notes/heddle", "show", git_commit])
            .current_dir(temp.path())
            .output()
            .expect("git notes show should run");
        assert!(
            notes.status.success(),
            "landed commit {git_commit} should have a heddle note in the bridge mirror; stderr: {}",
            String::from_utf8_lossy(&notes.stderr)
        );
        let note_body = String::from_utf8(notes.stdout).unwrap();
        assert!(
            note_body.contains("hd-"),
            "note for {git_commit} should embed a Heddle change id: {note_body}"
        );
    }
}

#[test]
fn test_parallel_heddle_threads_ship_with_one_stale_refresh_path_and_checkpoint_both() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle_adopt(temp.path());

    let auth_started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "start",
                "feature/auth",
                "--workspace",
                "auto",
            ],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let auth_path = std::path::PathBuf::from(auth_started["execution_path"].as_str().unwrap());
    std::fs::write(auth_path.join("auth.rs"), "auth work").unwrap();
    heddle(&["capture", "-m", "auth work"], Some(&auth_path)).unwrap();

    std::fs::write(temp.path().join("base.txt"), "base advanced").unwrap();
    heddle(&["capture", "-m", "advance main"], Some(temp.path())).unwrap();

    let auth_before_ship: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "thread", "show", "feature/auth"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(auth_before_ship["freshness"], "stale");
    let auth_refresh: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "thread", "refresh", "feature/auth"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(auth_refresh["status"], "completed");

    let search_started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "start",
                "feature/search",
                "--workspace",
                "auto",
            ],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let search_path = std::path::PathBuf::from(search_started["execution_path"].as_str().unwrap());
    std::fs::write(search_path.join("search.rs"), "search work").unwrap();
    heddle(&["capture", "-m", "search work"], Some(&search_path)).unwrap();

    let auth_ship: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "land", "--thread", "feature/auth"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(auth_ship["status"], "landed");
    assert_eq!(auth_ship["checkpointed"], true);
    assert!(auth_ship["git_commit"].as_str().is_some());

    let search_refresh: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "thread", "refresh", "feature/search"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(search_refresh["status"], "completed");

    let search_ship: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "land", "--thread", "feature/search"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(search_ship["status"], "landed");
    assert_eq!(search_ship["synced"], false);
    assert_eq!(search_ship["checkpointed"], true);
    assert!(search_ship["git_commit"].as_str().is_some());

    let auth_thread: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "thread", "show", "feature/auth"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(auth_thread["thread_state"], "merged");
    assert_eq!(
        auth_thread["integration_policy_result"]["status"],
        "auto_integrated"
    );

    let search_thread: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "thread", "show", "feature/search"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(search_thread["thread_state"], "merged");
    assert_eq!(
        search_thread["integration_policy_result"]["status"],
        "auto_integrated"
    );

    let checkpoint_records_path = temp.path().join(".heddle/state/git-checkpoints.json");
    let checkpoint_records: Value =
        serde_json::from_str(&std::fs::read_to_string(checkpoint_records_path).unwrap()).unwrap();
    let records = checkpoint_records.as_array().unwrap();
    assert!(
        records
            .iter()
            .any(|record| record["summary"] == "auth work"),
        "stale auth land should record a git checkpoint: {checkpoint_records}"
    );
    assert!(
        records
            .iter()
            .any(|record| record["summary"] == "search work"),
        "clean search land should record a git checkpoint: {checkpoint_records}"
    );
}

#[test]
fn test_cli_push_rejects_local_only_git_overlay_repo() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();

    // Fresh `git init` repos have no remote. `heddle push` should
    // fail with a clear pointer back at the missing destination
    // rather than silently no-op'ing.
    let err = heddle(&["--output", "json", "push"], Some(temp.path())).unwrap_err();
    assert!(
        err.contains("remote_not_configured")
            && err.contains("heddle remote add <name> <url>")
            && err.contains("heddle remote list"),
        "expected guidance about the missing remote, got: {err}"
    );
}

#[test]
fn test_cli_snapshot_no_agent_ignores_corrupt_session_state() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::create_dir_all(temp.path().join(".heddle/state")).unwrap();
    std::fs::write(
        temp.path().join(".heddle/state/worktree.toml"),
        "not = valid = toml",
    )
    .unwrap();
    std::fs::write(temp.path().join("hello.txt"), "world").unwrap();

    let output = heddle(
        &["capture", "--no-agent", "-m", "Human snapshot"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        output.contains("Created state") || output.contains("hd-"),
        "human snapshot should not require session state: {}",
        output
    );
}

#[test]
fn test_cli_snapshot_with_confidence() {
    let temp = TempDir::new().unwrap();

    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();

    unsafe {
        std::env::set_var("HEDDLE_AGENT_PROVIDER", "test");
        std::env::set_var("HEDDLE_AGENT_MODEL", "test-model");
    }

    let result = heddle(
        &[
            "capture",
            "--intent",
            "Test with confidence",
            "--confidence",
            "0.95",
        ],
        Some(temp.path()),
    );

    unsafe {
        std::env::remove_var("HEDDLE_AGENT_PROVIDER");
        std::env::remove_var("HEDDLE_AGENT_MODEL");
    }

    assert!(result.is_ok());
}

#[test]
fn test_cli_snapshot_without_confidence_records_none() {
    let temp = TempDir::new().unwrap();

    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();

    let output = heddle(
        &[
            "--output",
            "json",
            "capture",
            "--intent",
            "Test without confidence",
        ],
        Some(temp.path()),
    )
    .unwrap();
    let snapshot_json: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert!(
        snapshot_json["confidence"].is_null(),
        "snapshot output should expose absent confidence as null: {snapshot_json:#}"
    );

    let change_id = snapshot_json["change_id"].as_str().unwrap();
    let show_json = heddle(&["show", "--output", "json", change_id], Some(temp.path())).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&show_json).unwrap();
    assert!(
        parsed["confidence"].is_null(),
        "omitted confidence should be stored as null: {parsed:#}"
    );
}

#[test]
fn test_cli_log_shows_history() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for i in 1..=3 {
        std::fs::write(
            temp.path().join(format!("file{}.txt", i)),
            format!("content {}", i),
        )
        .unwrap();
        heddle(
            &["capture", "-m", &format!("Commit {}", i)],
            Some(temp.path()),
        )
        .unwrap();
    }

    let output = heddle(&["log"], Some(temp.path())).unwrap();
    assert!(
        output.contains("Commit 1") || output.contains("hd-"),
        "Should show commits: {}",
        output
    );
}

#[test]
fn test_cli_log_with_limit() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for i in 1..=5 {
        std::fs::write(
            temp.path().join(format!("file{}.txt", i)),
            format!("content {}", i),
        )
        .unwrap();
        heddle(
            &["capture", "-m", &format!("Commit {}", i)],
            Some(temp.path()),
        )
        .unwrap();
    }

    assert!(heddle(&["log", "--limit", "2"], Some(temp.path())).is_ok());
}

#[test]
fn test_cli_log_limit_caps_json_state_count() {
    // `--limit N` must trim the JSON `states` array to at most N
    // entries, regardless of how much history exists.
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    for i in 1..=6 {
        std::fs::write(
            temp.path().join(format!("file{}.txt", i)),
            format!("content {}", i),
        )
        .unwrap();
        heddle(
            &["capture", "-m", &format!("Commit {}", i)],
            Some(temp.path()),
        )
        .unwrap();
    }

    let json = heddle(
        &["--output", "json", "log", "--limit", "3"],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&json).unwrap();
    let states = parsed["states"].as_array().expect("states array");
    assert!(
        states.len() <= 3,
        "`--limit 3` should return at most 3 states, got {}: {}",
        states.len(),
        json
    );
}

#[test]
fn test_cli_log_since_marker_excludes_marker_and_walks_back() {
    // `--since <marker>` walks back until it reaches the marker's
    // state, then returns everything *above* it (newer than the
    // marker, excluding the marker's state itself).
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    // Capture three pre-marker states.
    for i in 1..=3 {
        std::fs::write(
            temp.path().join(format!("pre{}.txt", i)),
            format!("pre {}", i),
        )
        .unwrap();
        heddle(
            &["capture", "-m", &format!("Pre-marker {}", i)],
            Some(temp.path()),
        )
        .unwrap();
    }

    // Drop the marker at the current HEAD.
    heddle(
        &["thread", "marker", "create", "checkpoint"],
        Some(temp.path()),
    )
    .unwrap();

    // Capture two post-marker states.
    for i in 1..=2 {
        std::fs::write(
            temp.path().join(format!("post{}.txt", i)),
            format!("post {}", i),
        )
        .unwrap();
        heddle(
            &["capture", "-m", &format!("Post-marker {}", i)],
            Some(temp.path()),
        )
        .unwrap();
    }

    let json = heddle(
        &["--output", "json", "log", "--since", "checkpoint"],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&json).unwrap();
    let states = parsed["states"].as_array().expect("states array");

    // We expect exactly the two post-marker captures.
    assert_eq!(
        states.len(),
        2,
        "`--since checkpoint` should return 2 post-marker states, got: {}",
        json
    );
    let intents: Vec<&str> = states
        .iter()
        .map(|s| s["intent"].as_str().unwrap_or(""))
        .collect();
    assert!(
        intents.iter().any(|i| i.contains("Post-marker 2")),
        "should include Post-marker 2: {:?}",
        intents
    );
    assert!(
        !intents.iter().any(|i| i.contains("Pre-marker")),
        "should not include any Pre-marker states: {:?}",
        intents
    );
}

#[test]
fn test_cli_log_since_with_limit_applies_bound_then_trims() {
    // When `--since` and `--limit` are both set, the bound is applied
    // first (yielding "everything newer than the marker"), then the
    // result is trimmed to `--limit`. So `--limit 2 --since <marker>`
    // returns at most 2 entries even if more captures exist above the
    // bound.
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "marker", "create", "start"], Some(temp.path())).unwrap();

    for i in 1..=4 {
        std::fs::write(
            temp.path().join(format!("after{}.txt", i)),
            format!("after {}", i),
        )
        .unwrap();
        heddle(
            &["capture", "-m", &format!("After-{}", i)],
            Some(temp.path()),
        )
        .unwrap();
    }

    let json = heddle(
        &[
            "--output", "json", "log", "--since", "start", "--limit", "2",
        ],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&json).unwrap();
    let states = parsed["states"].as_array().expect("states array");
    assert_eq!(
        states.len(),
        2,
        "`--limit 2 --since start` should return exactly 2 states, got: {}",
        json
    );
}

#[test]
fn test_cli_show_state_details() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("test.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Test state"], Some(temp.path())).unwrap();

    let output = heddle(&["show", "HEAD"], Some(temp.path())).unwrap();
    assert!(
        output.contains("Test state") || output.contains("hd-"),
        "Should show state details: {}",
        output
    );
}

#[test]
fn test_cli_diff_shows_changes() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    std::fs::write(temp.path().join("file.txt"), "original").unwrap();
    heddle(&["capture", "-m", "Original"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "modified").unwrap();

    let output = heddle(&["diff"], Some(temp.path())).unwrap();
    assert!(
        output.contains("file.txt") || output.contains("modified") || output.contains("diff"),
        "Diff should show changes: {}",
        output
    );
}

#[test]
fn test_cli_diff_renders_unified_hunks_with_three_context_lines_by_default() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let original = (1..=9)
        .map(|line| format!("line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(temp.path().join("file.txt"), original).unwrap();
    heddle(&["capture", "-m", "Original"], Some(temp.path())).unwrap();

    let modified = (1..=9)
        .map(|line| {
            if line == 5 {
                "line five changed".to_string()
            } else {
                format!("line {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(temp.path().join("file.txt"), modified).unwrap();

    let output = heddle(&["--output", "text", "diff"], Some(temp.path())).unwrap();
    assert!(
        output.contains("@@"),
        "diff should include hunk headers: {output}"
    );
    assert!(
        output.contains(" line 2") && output.contains(" line 8"),
        "default unified diff should include three surrounding context lines: {output}"
    );
    assert!(
        !output.contains(" line 1") && !output.contains(" line 9"),
        "default unified diff should omit context outside the hunk: {output}"
    );
    assert!(
        output.contains("-line 5") && output.contains("+line five changed"),
        "no-color diff should preserve explicit old/new lines: {output}"
    );

    let tight = heddle(
        &["--output", "text", "diff", "--unified", "1"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        tight.contains(" line 4") && tight.contains(" line 6"),
        "--unified 1 should include one surrounding line: {tight}"
    );
    assert!(
        !tight.contains(" line 3") && !tight.contains(" line 7"),
        "--unified 1 should omit farther context: {tight}"
    );
}

#[cfg(feature = "semantic")]
#[test]
fn test_cli_diff_semantic_still_renders_text_hunks() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    std::fs::create_dir_all(temp.path().join("src")).unwrap();
    std::fs::write(
        temp.path().join("src/lib.rs"),
        "pub fn answer() -> i32 {\n    41\n}\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "Original"], Some(temp.path())).unwrap();

    std::fs::write(
        temp.path().join("src/lib.rs"),
        "pub fn answer() -> i32 {\n    42\n}\n",
    )
    .unwrap();

    let output = heddle(
        &["--output", "text", "diff", "--semantic"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        output.contains("--- a/src/lib.rs"),
        "missing file header: {output}"
    );
    assert!(
        output.contains("@@"),
        "semantic diff should include hunks: {output}"
    );
    assert!(output.contains("-    41"), "missing removed line: {output}");
    assert!(output.contains("+    42"), "missing added line: {output}");
    assert!(
        !output.contains("Binary file or unable to diff"),
        "semantic text diff should not fall back to binary message: {output}"
    );
}

#[test]
fn test_cli_diff_color_renders_modified_lines_as_single_tilde_row() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    std::fs::write(temp.path().join("file.txt"), "let value = 41;\n").unwrap();
    heddle(&["capture", "-m", "Original"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "let value = 42;\n").unwrap();

    let output = heddle_output_with_env(
        &["--output", "text", "diff", "--unified", "0"],
        Some(temp.path()),
        &[("CLICOLOR_FORCE", "1")],
    )
    .unwrap();
    assert!(output.status.success());
    let stdout = str::from_utf8(&output.stdout).unwrap_or("");
    assert!(
        stdout.contains("\x1b["),
        "forced color should emit ANSI: {stdout:?}"
    );
    assert!(
        stdout.contains("~") && !stdout.contains(" -> "),
        "colored modified line should be a single tilde row without arrow text: {stdout:?}"
    );
    assert!(
        !stdout.contains("-let value = 41;") && !stdout.contains("+let value = 42;"),
        "colored modified line should not render as delete/add churn: {stdout:?}"
    );
}

#[test]
fn test_cli_diff_stat_only() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Original"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "modified").unwrap();

    assert!(heddle(&["diff", "--stat"], Some(temp.path())).is_ok());
}

#[test]
fn test_cli_goto_changes_worktree() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    std::fs::write(temp.path().join("version.txt"), "v1").unwrap();
    heddle(&["capture", "-m", "Version 1"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("version.txt"), "v2").unwrap();
    heddle(&["capture", "-m", "Version 2"], Some(temp.path())).unwrap();

    let content = std::fs::read_to_string(temp.path().join("version.txt")).unwrap();
    assert_eq!(content, "v2");

    assert!(heddle(&["switch", "HEAD~1"], Some(temp.path())).is_ok());
    let content = std::fs::read_to_string(temp.path().join("version.txt")).unwrap();
    assert_eq!(content, "v1", "File should be restored to v1");
}

#[test]
fn test_cli_undo_redo() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "State 1"], Some(temp.path())).unwrap();
    let head_after_first = status_json(temp.path());
    let first_id = head_after_first["state"]["change_id"]
        .as_str()
        .expect("state change_id should be string")
        .to_string();
    assert_eq!(head_after_first["thread"].as_str().unwrap_or(""), "main");

    std::fs::write(temp.path().join("file.txt"), "updated").unwrap();
    heddle(&["capture", "-m", "State 2"], Some(temp.path())).unwrap();
    let head_after_second = status_json(temp.path());
    let second_id = head_after_second["state"]["change_id"]
        .as_str()
        .expect("state change_id should be string")
        .to_string();

    assert!(heddle(&["undo"], Some(temp.path())).is_ok());
    let head_after_undo = status_json(temp.path());
    let undo_id = head_after_undo["state"]["change_id"]
        .as_str()
        .expect("state change_id should be string")
        .to_string();
    assert_eq!(head_after_undo["thread"].as_str().unwrap_or(""), "main");
    assert_eq!(undo_id, first_id, "Undo should move to previous state");

    assert!(heddle(&["undo", "--redo"], Some(temp.path())).is_ok());
    let head_after_redo = status_json(temp.path());
    let redo_id = head_after_redo["state"]["change_id"]
        .as_str()
        .expect("state change_id should be string")
        .to_string();
    assert_eq!(head_after_redo["thread"].as_str().unwrap_or(""), "main");
    assert_eq!(redo_id, second_id, "Redo should restore latest state");
}

/// `heddle show` and `heddle log` must distinguish an unset
/// confidence (`None`) from a low-confidence value. Render the
/// absent case as `Confidence: —` (em dash) and never as `0.00`,
/// which would silently lie about a value the agent never asserted.
///
/// We bypass the `cmd_snapshot` path on purpose: that path layers in
/// the user-config / repo-defaults fallback (0.8), so a `None`
/// confidence can only originate from a non-snapshot writer such as
/// the git bridge import. Putting the state directly via the object
/// store is the smallest reliable way to reproduce that scenario in
/// a CLI integration test.
#[test]
fn test_cli_show_renders_absent_confidence_as_em_dash() {
    use objects::object::{Attribution, Principal, State, Tree};
    use repo::Repository;

    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).expect("init repo");

    let tree = Tree::new();
    let tree_hash = repo.store().put_tree(&tree).expect("put tree");
    let attribution = Attribution::human(Principal::new("Test", "test@example.com"));
    let state = State::new(tree_hash, vec![], attribution).with_intent("imported state");
    assert!(
        state.confidence.is_none(),
        "fixture must have None confidence so the test exercises the absent branch",
    );
    repo.store().put_state(&state).expect("put state");
    let short_id = state.change_id.short();
    // Advance the seeded `main` thread to our `None`-confidence state so
    // `heddle log` (which walks from HEAD) actually traverses it.
    repo.refs()
        .set_thread(&ThreadName::new("main"), &state.change_id)
        .expect("set main thread");
    drop(repo);

    // Text mode: must show `Confidence: —`, never `Confidence: 0.00`.
    // The integration harness runs the binary as a subprocess, so the
    // auto-detect output format would otherwise pick JSON; force text.
    let show_text =
        heddle(&["--output", "text", "show", &short_id], Some(temp.path())).expect("heddle show");
    assert!(
        show_text.contains("Confidence: —"),
        "show should render an em dash for absent confidence; got:\n{show_text}"
    );
    assert!(
        !show_text.contains("Confidence: 0.00"),
        "show must not render absent confidence as 0.00; got:\n{show_text}"
    );
    assert!(
        !show_text.contains("Confidence: 0%"),
        "show must not render absent confidence as 0%; got:\n{show_text}"
    );

    // JSON mode: an `Option<f32>` field with no `skip_serializing_if`
    // serializes as `null`, and the web app reads it via `?? null`.
    let show_json_str =
        heddle(&["--output", "json", "show", &short_id], Some(temp.path())).expect("show json");
    let show_json: serde_json::Value =
        serde_json::from_str(&show_json_str).expect("show JSON parses");
    assert!(
        show_json["confidence"].is_null(),
        "JSON confidence must be null for absent value, got {show_json:#}"
    );

    // `heddle log` is the high-density, multi-state surface: rendering
    // `Confidence: —` on every entry stacked a noise tax that hurt
    // readability without communicating new information (the absence of
    // a Confidence line already says "no value asserted"). The contract
    // it preserves is the same as `show`: never silently substitute a
    // numeric 0.00 / 0% for an unset confidence. JSON still serializes
    // `confidence: null` so agents distinguish the cases.
    let log_text = heddle(&["--output", "text", "log"], Some(temp.path())).expect("heddle log");
    assert!(
        !log_text.contains("Confidence: 0.00"),
        "log must not render absent confidence as 0.00; got:\n{log_text}"
    );
    assert!(
        !log_text.contains("Confidence: 0%"),
        "log must not render absent confidence as 0%; got:\n{log_text}"
    );
    // The state should still be visible — only the per-entry confidence
    // line is suppressed when unset.
    assert!(
        log_text.contains("imported state"),
        "the absent-confidence state should still appear in the log; got:\n{log_text}"
    );
}

/// `heddle diff --patch` must emit a clean unified-diff body — no gutter,
/// no `~`-flagged modified-pair lines, standard `--- a/`/`+++ b/`/`@@`
/// headers. The default rendering (with the line-number gutter) is great
/// for humans and useless to `patch(1)`/`git apply`/AI pair tools; the
/// flag carves out a machine-friendly mode without touching the default.
#[test]
fn test_cli_diff_patch_flag_emits_clean_unified_diff() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    let original = "line one\nline two\nline three\nline four\nline five\n";
    std::fs::write(temp.path().join("file.txt"), original).unwrap();
    heddle(&["capture", "-m", "v1"], Some(temp.path())).unwrap();
    let modified = "line one\nline TWO\nline three\nline four\nline five\n";
    std::fs::write(temp.path().join("file.txt"), modified).unwrap();

    let patch = heddle(&["diff", "--patch"], Some(temp.path())).unwrap();

    assert!(
        !patch.lines().any(|line| line.contains(" | ")),
        "patch output must not carry the prettified gutter: {patch}"
    );
    assert!(
        !patch.contains("\x1b["),
        "patch output must not carry ANSI styling: {patch:?}"
    );
    assert!(
        patch.contains("--- a/file.txt"),
        "patch output must carry the standard `--- a/<path>` header: {patch}"
    );
    assert!(
        patch.contains("+++ b/file.txt"),
        "patch output must carry the standard `+++ b/<path>` header: {patch}"
    );
    assert!(
        patch.contains("@@ "),
        "patch output must carry the `@@` hunk header: {patch}"
    );
    assert!(
        patch.contains("-line two"),
        "patch output must carry the removed line with a `-` prefix: {patch}"
    );
    assert!(
        patch.contains("+line TWO"),
        "patch output must carry the added line with a `+` prefix: {patch}"
    );
}

/// `-p` is the short alias for `--patch`, matching `git diff -p`.
#[test]
fn test_cli_diff_patch_short_alias_matches_long_form() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "alpha\nbeta\ngamma\n").unwrap();
    heddle(&["capture", "-m", "v1"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "alpha\nBETA\ngamma\n").unwrap();

    let long_form = heddle(&["diff", "--patch"], Some(temp.path())).unwrap();
    let short_form = heddle(&["diff", "-p"], Some(temp.path())).unwrap();

    assert_eq!(
        long_form, short_form,
        "`-p` must be a pure alias for `--patch`"
    );
}

/// `heddle diff` (no flag) must keep its prettified gutter. The
/// machine-friendly mode is strictly opt-in; flipping the default would
/// break the human reading loop the gutter exists to serve.
#[test]
fn test_cli_diff_default_retains_line_number_gutter() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "alpha\nbeta\ngamma\n").unwrap();
    heddle(&["capture", "-m", "v1"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "alpha\nBETA\ngamma\n").unwrap();

    let default = heddle(&["diff"], Some(temp.path())).unwrap();

    assert!(
        default.lines().any(|line| line.contains(" | ")),
        "default `heddle diff` must keep its line-number gutter: {default}"
    );
}

/// `git apply --check` must accept the `--patch` output for a simple
/// modify. The point of the flag: round-trip through standard tools.
#[test]
fn test_cli_diff_patch_output_applies_with_git_apply() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    let original = "line one\nline two\nline three\nline four\nline five\n";
    std::fs::write(temp.path().join("file.txt"), original).unwrap();
    heddle(&["capture", "-m", "v1"], Some(temp.path())).unwrap();
    let modified = "line one\nline TWO\nline three\nline four\nline five\n";
    std::fs::write(temp.path().join("file.txt"), modified).unwrap();

    let patch = heddle(&["diff", "--patch"], Some(temp.path())).unwrap();

    let apply_dir = TempDir::new().unwrap();
    init_git_repo(apply_dir.path());
    std::fs::write(apply_dir.path().join("file.txt"), original).unwrap();
    git_commit_all(apply_dir.path(), "seed pre-patch content");

    let mut child = Command::new("git")
        .args(["apply", "--check"])
        .current_dir(apply_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git apply should spawn");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("git apply should finish");
    assert!(
        out.status.success(),
        "git apply --check should accept --patch output;\npatch=\n{patch}\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `patch(1) -p1 --dry-run` must accept the `--patch` output. This is
/// the canonical "copy the diff into the chat, ask the AI to apply it"
/// loop — it's broken today and the flag exists to fix it.
#[test]
fn test_cli_diff_patch_output_applies_with_patch_command() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    let original = "line one\nline two\nline three\nline four\nline five\n";
    std::fs::write(temp.path().join("file.txt"), original).unwrap();
    heddle(&["capture", "-m", "v1"], Some(temp.path())).unwrap();
    let modified = "line one\nline TWO\nline three\nline four\nline five\n";
    std::fs::write(temp.path().join("file.txt"), modified).unwrap();

    let patch = heddle(&["diff", "--patch"], Some(temp.path())).unwrap();

    let apply_dir = TempDir::new().unwrap();
    std::fs::write(apply_dir.path().join("file.txt"), original).unwrap();

    let mut child = Command::new("patch")
        .args(["-p1", "--dry-run"])
        .current_dir(apply_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("patch should spawn");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("patch should finish");
    assert!(
        out.status.success(),
        "patch -p1 --dry-run should accept --patch output;\npatch=\n{patch}\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// When the original blob lacks a trailing newline, `--patch` must
/// emit the standard `\ No newline at end of file` marker on the OLD
/// side. Without it, `diff_blobs` strips terminators and the renderer
/// would synthesise a `\n` onto the `-` line that no longer matches
/// the real source — `git apply --check` rejects the resulting patch.
#[test]
fn test_cli_diff_patch_preserves_missing_old_final_newline() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    // No trailing newline — common for generated configs and
    // single-line scripts.
    std::fs::write(temp.path().join("noeol.txt"), b"hello").unwrap();
    heddle(&["capture", "-m", "v1"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("noeol.txt"), "hello\nmore\n").unwrap();

    let patch = heddle(&["diff", "--patch"], Some(temp.path())).unwrap();

    assert!(
        patch.contains("\\ No newline at end of file"),
        "OLD side lacked a trailing newline; patch must carry the marker:\n{patch}"
    );

    let apply_dir = TempDir::new().unwrap();
    init_git_repo(apply_dir.path());
    std::fs::write(apply_dir.path().join("noeol.txt"), b"hello").unwrap();
    git_commit_all(apply_dir.path(), "seed no-eol content");

    let mut child = Command::new("git")
        .args(["apply", "--check"])
        .current_dir(apply_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git apply should spawn");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("git apply should finish");
    assert!(
        out.status.success(),
        "git apply --check must accept a no-eol-side patch;\npatch=\n{patch}\nstderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The mirror case: the NEW blob lacks a trailing newline. The marker
/// must land on the NEW side so the patch describes a file that ends
/// without `\n` once applied.
#[test]
fn test_cli_diff_patch_preserves_missing_new_final_newline() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("noeol.txt"), "hello\nmore\n").unwrap();
    heddle(&["capture", "-m", "v1"], Some(temp.path())).unwrap();
    // Strip the trailing newline AND the last line — common when
    // turning a multi-line script into a one-liner.
    std::fs::write(temp.path().join("noeol.txt"), b"hello").unwrap();

    let patch = heddle(&["diff", "--patch"], Some(temp.path())).unwrap();

    assert!(
        patch.contains("\\ No newline at end of file"),
        "NEW side lacks a trailing newline; patch must carry the marker:\n{patch}"
    );

    let apply_dir = TempDir::new().unwrap();
    init_git_repo(apply_dir.path());
    std::fs::write(apply_dir.path().join("noeol.txt"), "hello\nmore\n").unwrap();
    git_commit_all(apply_dir.path(), "seed with-eol content");

    let mut child = Command::new("git")
        .args(["apply", "--check"])
        .current_dir(apply_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git apply should spawn");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("git apply should finish");
    assert!(
        out.status.success(),
        "git apply --check must accept a no-eol-mirror patch;\npatch=\n{patch}\nstderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `heddle diff --patch` from a plain-Git checkout (no `heddle init`)
/// must emit a real unified-diff body. The status-only fast path
/// constructed every `FileChange` with `lines: None`, so the patch
/// printer skipped them all and produced an empty string — useless
/// to `patch(1)` / `git apply`. When `--patch` is requested we now
/// delegate to `git diff -p` for the body.
#[test]
fn test_cli_diff_patch_works_on_plain_git_fast_path() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    let original = "line one\nline two\nline three\nline four\nline five\n";
    std::fs::write(temp.path().join("file.txt"), original).unwrap();
    git_commit_all(temp.path(), "seed plain-git content");
    let modified = "line one\nline TWO\nline three\nline four\nline five\n";
    std::fs::write(temp.path().join("file.txt"), modified).unwrap();

    let patch = heddle(&["diff", "--patch"], Some(temp.path())).unwrap();

    assert!(
        !patch.trim().is_empty(),
        "plain-Git fast path must emit a non-empty patch body when files changed; got:\n{patch:?}"
    );
    assert!(
        patch.contains("-line two") && patch.contains("+line TWO"),
        "patch body must include the actual edit:\n{patch}"
    );

    let apply_dir = TempDir::new().unwrap();
    std::fs::write(apply_dir.path().join("file.txt"), original).unwrap();
    let mut child = Command::new("patch")
        .args(["-p1", "--dry-run"])
        .current_dir(apply_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("patch should spawn");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("patch should finish");
    assert!(
        out.status.success(),
        "patch -p1 --dry-run must accept the plain-Git fast-path body;\npatch=\n{patch}\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Plain-Git `--patch` for a NEW file (no entry in HEAD, present in
/// the worktree) must emit a `--- /dev/null`-style add hunk via the
/// gix path. The status-only fast path used to silently drop the body
/// for every kind, not just modified, so the added branch needs its
/// own coverage. We additionally assert the patch applies through
/// `git apply --check` against a clone of the seed state.
#[test]
fn test_cli_diff_patch_plain_git_added_file_emits_hunk() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("kept.txt"), "anchor\n").unwrap();
    git_commit_all(temp.path(), "seed");
    std::fs::write(temp.path().join("new.txt"), "alpha\nbeta\n").unwrap();
    // git status sees an untracked file as "added" only once staged.
    git(&["add", "new.txt"], temp.path());

    let patch = heddle(&["diff", "--patch"], Some(temp.path())).unwrap();

    assert!(
        patch.contains("+++ b/new.txt"),
        "ADDED file must produce a `+++ b/<path>` header:\n{patch}"
    );
    assert!(
        patch.contains("+alpha") && patch.contains("+beta"),
        "ADDED file body must include every new line:\n{patch}"
    );

    let apply_dir = TempDir::new().unwrap();
    init_git_repo(apply_dir.path());
    std::fs::write(apply_dir.path().join("kept.txt"), "anchor\n").unwrap();
    git_commit_all(apply_dir.path(), "seed apply");

    let mut child = Command::new("git")
        .args(["apply", "--check"])
        .current_dir(apply_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git apply should spawn");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("git apply should finish");
    assert!(
        out.status.success(),
        "git apply --check must accept a plain-Git add patch;\npatch=\n{patch}\nstderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Plain-Git `--patch` for a DELETED file (present in HEAD, missing
/// from the worktree) must emit the git-canonical deletion block:
/// `deleted file mode` + `--- a/<path>` + `+++ /dev/null` + an
/// all-`-` hunk. `git apply --check` tolerates a bare `+++ b/<path>`
/// shape, but actually applying it would truncate the file to empty
/// instead of removing it — so we apply for real and assert the path
/// is gone, not left behind as a zero-byte file.
#[test]
fn test_cli_diff_patch_plain_git_deleted_file_round_trips() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("doomed.txt"), "gamma\ndelta\n").unwrap();
    std::fs::write(temp.path().join("kept.txt"), "anchor\n").unwrap();
    git_commit_all(temp.path(), "seed");
    std::fs::remove_file(temp.path().join("doomed.txt")).unwrap();
    git(&["add", "-A"], temp.path());

    let patch = heddle(&["diff", "--patch"], Some(temp.path())).unwrap();

    assert!(
        patch.contains("deleted file mode 100644"),
        "DELETED file must carry the `deleted file mode` header:\n{patch}"
    );
    assert!(
        patch.contains("--- a/doomed.txt"),
        "DELETED file must produce a `--- a/<path>` header:\n{patch}"
    );
    assert!(
        patch.contains("+++ /dev/null"),
        "DELETED file must source the new side from `/dev/null`:\n{patch}"
    );
    assert!(
        !patch.contains("+++ b/doomed.txt"),
        "DELETED file must NOT emit `+++ b/<path>` (that truncates, not removes):\n{patch}"
    );
    assert!(
        patch.contains("-gamma") && patch.contains("-delta"),
        "DELETED file body must include every removed line:\n{patch}"
    );

    let apply_dir = TempDir::new().unwrap();
    init_git_repo(apply_dir.path());
    std::fs::write(apply_dir.path().join("doomed.txt"), "gamma\ndelta\n").unwrap();
    std::fs::write(apply_dir.path().join("kept.txt"), "anchor\n").unwrap();
    git_commit_all(apply_dir.path(), "seed apply");

    let mut check = Command::new("git")
        .args(["apply", "--check"])
        .current_dir(apply_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git apply should spawn");
    check
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch.as_bytes())
        .unwrap();
    let check_out = check.wait_with_output().expect("git apply should finish");
    assert!(
        check_out.status.success(),
        "git apply --check must accept a plain-Git delete patch;\npatch=\n{patch}\nstderr={}",
        String::from_utf8_lossy(&check_out.stderr)
    );

    // Apply for real and confirm the path is unlinked rather than
    // truncated to a zero-byte file — the actual bug behind cid 3315303999.
    let mut apply = Command::new("git")
        .args(["apply"])
        .current_dir(apply_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git apply should spawn");
    apply
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch.as_bytes())
        .unwrap();
    let apply_out = apply.wait_with_output().expect("git apply should finish");
    assert!(
        apply_out.status.success(),
        "git apply must apply a plain-Git delete patch;\npatch=\n{patch}\nstderr={}",
        String::from_utf8_lossy(&apply_out.stderr)
    );
    assert!(
        !apply_dir.path().join("doomed.txt").exists(),
        "applying the delete patch must REMOVE doomed.txt, not leave it behind:\n{patch}"
    );
    assert!(
        apply_dir.path().join("kept.txt").exists(),
        "applying the delete patch must leave untouched files in place:\n{patch}"
    );
}

/// `heddle --output json diff` from a plain-Git checkout (no `heddle
/// init`) must carry the rendered unified diff in the top-level
/// `patch` field — same contract as the heddle path. Without this,
/// structured consumers reading the JSON would have to reconstruct
/// the patch from the per-line array, defeating the field's purpose.
#[test]
fn test_cli_diff_json_plain_git_patch_field_round_trips() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    let original = "a\nb\nc\n";
    std::fs::write(temp.path().join("file.txt"), original).unwrap();
    git_commit_all(temp.path(), "seed plain-git json");
    let modified = "a\nB\nc\n";
    std::fs::write(temp.path().join("file.txt"), modified).unwrap();

    let output = heddle_output(&["--output", "json", "diff", "--patch"], Some(temp.path()))
        .expect("heddle diff --patch should run");
    assert!(
        output.status.success(),
        "heddle diff --patch should succeed; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value = json_stdout(&output, "plain-git --patch json");
    let patch_field = value
        .get("patch")
        .and_then(Value::as_str)
        .expect("JSON output must include top-level `patch` field");
    assert!(
        patch_field.contains("--- a/file.txt"),
        "patch field must carry the unified-diff body: {patch_field}"
    );
    assert!(
        patch_field.contains("-b") && patch_field.contains("+B"),
        "patch field must include the actual edit: {patch_field}"
    );
}

/// The JSON `.patch` field is populated for the heddle (state-to-worktree)
/// path too, even when the CLI flag is the default (no `--patch`). This
/// is the round-trip contract for structured consumers: they should
/// never have to reassemble the patch from the per-line array.
#[test]
fn test_cli_diff_json_heddle_patch_field_present_without_flag() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "x\ny\nz\n").unwrap();
    heddle(&["capture", "-m", "v1"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "x\nY\nz\n").unwrap();

    let output =
        heddle_output(&["--output", "json", "diff"], Some(temp.path())).expect("heddle should run");
    assert!(output.status.success());
    let value = json_stdout(&output, "heddle diff json");
    let patch_field = value
        .get("patch")
        .and_then(Value::as_str)
        .expect("JSON output must include top-level `patch` field");
    assert!(
        patch_field.contains("--- a/file.txt") && patch_field.contains("+++ b/file.txt"),
        "patch field must carry the standard headers: {patch_field}"
    );
}

/// A pure rename (no content change) must round-trip through
/// `git apply --check` via the extended-header form. Without
/// `diff --git` + `similarity index` + `rename from`/`to`, `git apply`
/// treats `+++ b/<new>` as a path that must already exist on the
/// target side and rejects the patch as malformed.
#[test]
fn test_cli_diff_patch_pure_rename_round_trips() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    let body = "alpha\nbeta\ngamma\ndelta\nepsilon\n";
    std::fs::write(temp.path().join("from.txt"), body).unwrap();
    heddle(&["capture", "-m", "v1"], Some(temp.path())).unwrap();
    // Pure rename: same bytes, new path.
    std::fs::remove_file(temp.path().join("from.txt")).unwrap();
    std::fs::write(temp.path().join("to.txt"), body).unwrap();

    let patch = heddle(&["diff", "--patch"], Some(temp.path())).unwrap();

    assert!(
        patch.contains("diff --git a/from.txt b/to.txt"),
        "rename must emit `diff --git` extended header:\n{patch}"
    );
    assert!(
        patch.contains("similarity index 100%"),
        "pure rename must report 100% similarity:\n{patch}"
    );
    assert!(
        patch.contains("rename from from.txt") && patch.contains("rename to to.txt"),
        "rename must emit `rename from`/`rename to` headers:\n{patch}"
    );
    // Pure rename has no hunk body — `--- /+++/@@` would tell git to
    // apply an empty patch and warn.
    assert!(
        !patch.contains("--- a/from.txt") && !patch.contains("+++ b/to.txt"),
        "pure rename must not emit `--- /+++` lines:\n{patch}"
    );

    let apply_dir = TempDir::new().unwrap();
    init_git_repo(apply_dir.path());
    std::fs::write(apply_dir.path().join("from.txt"), body).unwrap();
    git_commit_all(apply_dir.path(), "seed rename source");

    let mut child = Command::new("git")
        .args(["apply", "--check"])
        .current_dir(apply_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git apply should spawn");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("git apply should finish");
    assert!(
        out.status.success(),
        "git apply --check must accept a pure-rename patch;\npatch=\n{patch}\nstderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A rename combined with an edit must emit BOTH the extended rename
/// headers AND a hunk body. Without the headers `git apply` looks for
/// `b/<new>` on the target side and fails; without the body the edits
/// don't land.
#[test]
fn test_cli_diff_patch_rename_with_edit_round_trips() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    let original = "alpha\nbeta\ngamma\ndelta\nepsilon\n";
    std::fs::write(temp.path().join("source.txt"), original).unwrap();
    heddle(&["capture", "-m", "v1"], Some(temp.path())).unwrap();
    // Rename + tweak one line — still >75% LCS overlap so the rename
    // detector pairs them.
    std::fs::remove_file(temp.path().join("source.txt")).unwrap();
    let edited = "alpha\nBETA\ngamma\ndelta\nepsilon\n";
    std::fs::write(temp.path().join("target.txt"), edited).unwrap();

    let patch = heddle(&["diff", "--patch"], Some(temp.path())).unwrap();

    assert!(
        patch.contains("diff --git a/source.txt b/target.txt"),
        "rename+edit must emit `diff --git` extended header:\n{patch}"
    );
    assert!(
        patch.contains("rename from source.txt") && patch.contains("rename to target.txt"),
        "rename+edit must emit `rename from`/`rename to` headers:\n{patch}"
    );
    // Similarity must be below 100% (one line changed) but well above
    // the detector's 75% floor.
    assert!(
        patch.contains("similarity index ") && !patch.contains("similarity index 100%"),
        "rename+edit must report a non-100% similarity:\n{patch}"
    );
    assert!(
        patch.contains("--- a/source.txt") && patch.contains("+++ b/target.txt"),
        "rename+edit must still emit the `--- /+++` line-diff headers:\n{patch}"
    );
    assert!(
        patch.contains("-beta") && patch.contains("+BETA"),
        "rename+edit body must include the actual edit:\n{patch}"
    );

    let apply_dir = TempDir::new().unwrap();
    init_git_repo(apply_dir.path());
    std::fs::write(apply_dir.path().join("source.txt"), original).unwrap();
    git_commit_all(apply_dir.path(), "seed rename+edit source");

    let mut child = Command::new("git")
        .args(["apply", "--check"])
        .current_dir(apply_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git apply should spawn");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("git apply should finish");
    assert!(
        out.status.success(),
        "git apply --check must accept a rename+edit patch;\npatch=\n{patch}\nstderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `heddle diff --patch` from an unborn plain-Git repo (no commits,
/// staged file) must render an add hunk against `/dev/null` instead of
/// erroring on the missing HEAD tree. The plain-Git fast path used to
/// call `head_tree()?` unconditionally — fine for an established repo,
/// fatal for a fresh `git init` where the only honest diff is "every
/// file is new."
#[test]
fn test_cli_diff_patch_plain_git_unborn_head_emits_add_hunk() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    // No commit — HEAD is unborn. Stage the new file so the probe
    // sees it (untracked-only files aren't reported as added).
    std::fs::write(temp.path().join("first.txt"), "alpha\nbeta\n").unwrap();
    git(&["add", "first.txt"], temp.path());

    let patch = heddle(&["diff", "--patch"], Some(temp.path())).unwrap();

    assert!(
        !patch.trim().is_empty(),
        "unborn-HEAD --patch must emit a non-empty body; got:\n{patch:?}"
    );
    assert!(
        patch.contains("diff --git a/first.txt b/first.txt"),
        "unborn-HEAD add patch must carry the `diff --git` extended header:\n{patch}"
    );
    assert!(
        patch.contains("new file mode 100644"),
        "unborn-HEAD add patch must carry the `new file mode` header:\n{patch}"
    );
    assert!(
        patch.contains("--- /dev/null"),
        "unborn-HEAD add patch must source from `/dev/null`:\n{patch}"
    );
    assert!(
        patch.contains("+++ b/first.txt"),
        "unborn-HEAD add patch must target `+++ b/<path>`:\n{patch}"
    );
    assert!(
        patch.contains("+alpha") && patch.contains("+beta"),
        "unborn-HEAD add patch body must include every new line:\n{patch}"
    );

    // Round-trip: apply against an empty target.
    let apply_dir = TempDir::new().unwrap();
    init_git_repo(apply_dir.path());
    // The target needs at least one commit so `git apply --check` has
    // a baseline; the file under test is absent, which is the case
    // the new-file mode header is designed for.
    std::fs::write(apply_dir.path().join("anchor.txt"), "anchor\n").unwrap();
    git_commit_all(apply_dir.path(), "seed apply baseline");

    let mut child = Command::new("git")
        .args(["apply", "--check"])
        .current_dir(apply_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git apply should spawn");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("git apply should finish");
    assert!(
        out.status.success(),
        "git apply --check must accept an unborn-HEAD add patch;\npatch=\n{patch}\nstderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `--name-only` must NOT trigger patch rendering — the printer only
/// reads `change.path`. Asserting "no headers, no `@@`, file paths
/// only" pins the disjointness from `--patch` so a future refactor
/// that accidentally always populates the patch text still doesn't
/// leak it into the name-only printer.
#[test]
fn test_cli_diff_name_only_does_not_emit_patch_body() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("a.txt"), "alpha\n").unwrap();
    std::fs::write(temp.path().join("b.txt"), "beta\n").unwrap();
    heddle(&["capture", "-m", "v1"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("a.txt"), "ALPHA\n").unwrap();
    std::fs::write(temp.path().join("b.txt"), "BETA\n").unwrap();

    let listing = heddle(&["diff", "--name-only"], Some(temp.path())).unwrap();

    assert!(
        !listing.contains("--- a/") && !listing.contains("+++ b/") && !listing.contains("@@"),
        "name-only output must not include unified-diff headers:\n{listing}"
    );
    assert!(
        listing.contains("a.txt") && listing.contains("b.txt"),
        "name-only output must still list each changed path:\n{listing}"
    );
}

/// Adding an empty tracked file must still produce a patch: git emits
/// the `new file mode` extended header with no hunk body and `git apply`
/// creates the file from that alone. The old `has_hunk_body` blanket
/// skip dropped the change entirely (empty blob -> empty hunk vector ->
/// no header), so applying the diff never created the file.
#[test]
fn test_cli_diff_patch_empty_file_add_round_trips() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("seed.txt"), "seed\n").unwrap();
    heddle(&["capture", "-m", "v1"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("empty.txt"), "").unwrap();

    let patch = heddle(&["diff", "--patch"], Some(temp.path())).unwrap();
    assert!(
        patch.contains("diff --git a/empty.txt b/empty.txt"),
        "empty add must carry the `diff --git` header:\n{patch}"
    );
    assert!(
        patch.contains("new file mode 100644"),
        "empty add must carry the `new file mode` header:\n{patch}"
    );
    assert!(
        !patch.contains("@@"),
        "empty add is header-only — no hunk body (matches git):\n{patch}"
    );

    let apply_dir = TempDir::new().unwrap();
    init_git_repo(apply_dir.path());
    std::fs::write(apply_dir.path().join("seed.txt"), "seed\n").unwrap();
    git_commit_all(apply_dir.path(), "seed");
    git_apply(apply_dir.path(), &patch);
    assert!(
        apply_dir.path().join("empty.txt").exists(),
        "applying the empty-add patch must create the file:\n{patch}"
    );
}

/// Deleting an empty tracked file mirrors the empty-add case: the
/// `deleted file mode` header alone is a valid patch and `git apply`
/// unlinks the path from it.
#[test]
fn test_cli_diff_patch_empty_file_delete_round_trips() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("willdie.txt"), "").unwrap();
    std::fs::write(temp.path().join("keep.txt"), "keep\n").unwrap();
    heddle(&["capture", "-m", "v1"], Some(temp.path())).unwrap();
    std::fs::remove_file(temp.path().join("willdie.txt")).unwrap();

    let patch = heddle(&["diff", "--patch"], Some(temp.path())).unwrap();
    assert!(
        patch.contains("diff --git a/willdie.txt b/willdie.txt"),
        "empty delete must carry the `diff --git` header:\n{patch}"
    );
    assert!(
        patch.contains("deleted file mode 100644"),
        "empty delete must carry the `deleted file mode` header:\n{patch}"
    );

    let apply_dir = TempDir::new().unwrap();
    init_git_repo(apply_dir.path());
    std::fs::write(apply_dir.path().join("willdie.txt"), "").unwrap();
    std::fs::write(apply_dir.path().join("keep.txt"), "keep\n").unwrap();
    git_commit_all(apply_dir.path(), "seed");
    git_apply(apply_dir.path(), &patch);
    assert!(
        !apply_dir.path().join("willdie.txt").exists(),
        "applying the empty-delete patch must unlink the file:\n{patch}"
    );
    assert!(
        apply_dir.path().join("keep.txt").exists(),
        "the empty-delete patch must leave other files alone:\n{patch}"
    );
}

/// An added executable must carry `new file mode 100755` so `git apply`
/// restores the exec bit. Hard-coding `100644` silently dropped the
/// executable mode on round-trip.
#[cfg(unix)]
#[test]
fn test_cli_diff_patch_added_executable_preserves_mode() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("seed.txt"), "seed\n").unwrap();
    heddle(&["capture", "-m", "v1"], Some(temp.path())).unwrap();
    let script = temp.path().join("run.sh");
    std::fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let patch = heddle(&["diff", "--patch"], Some(temp.path())).unwrap();
    assert!(
        patch.contains("new file mode 100755"),
        "executable add must carry `new file mode 100755`:\n{patch}"
    );

    let apply_dir = TempDir::new().unwrap();
    init_git_repo(apply_dir.path());
    std::fs::write(apply_dir.path().join("seed.txt"), "seed\n").unwrap();
    git_commit_all(apply_dir.path(), "seed");
    git_apply(apply_dir.path(), &patch);
    let applied = apply_dir.path().join("run.sh");
    let mode = std::fs::metadata(&applied).unwrap().permissions().mode();
    assert!(
        mode & 0o111 != 0,
        "applied run.sh must be executable; mode={mode:o}"
    );
}

/// An added symlink must carry `new file mode 120000` and a hunk body
/// that is the link target (git stores a symlink as a blob containing
/// its target). `git apply` then recreates the symlink, not a regular
/// file holding the target text.
#[cfg(unix)]
#[test]
fn test_cli_diff_patch_added_symlink_preserves_mode_and_target() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("seed.txt"), "seed\n").unwrap();
    heddle(&["capture", "-m", "v1"], Some(temp.path())).unwrap();
    std::os::unix::fs::symlink("target/path", temp.path().join("linky")).unwrap();

    let patch = heddle(&["diff", "--patch"], Some(temp.path())).unwrap();
    assert!(
        patch.contains("new file mode 120000"),
        "symlink add must carry `new file mode 120000`:\n{patch}"
    );
    assert!(
        patch.contains("+target/path"),
        "symlink add body must be the link target:\n{patch}"
    );

    let apply_dir = TempDir::new().unwrap();
    init_git_repo(apply_dir.path());
    std::fs::write(apply_dir.path().join("seed.txt"), "seed\n").unwrap();
    git_commit_all(apply_dir.path(), "seed");
    git_apply(apply_dir.path(), &patch);
    let applied = apply_dir.path().join("linky");
    let meta = std::fs::symlink_metadata(&applied).unwrap();
    assert!(
        meta.file_type().is_symlink(),
        "applying the symlink-add patch must recreate a symlink"
    );
    assert_eq!(
        std::fs::read_link(&applied).unwrap().to_string_lossy(),
        "target/path",
        "the recreated symlink must point at the original target"
    );
}

/// A deleted file nested below a directory must resolve its old blob
/// through the recursive tree lookup. The old root-only `tree.get(path)`
/// missed `src/nested/file.txt`, so the deletion hunk was dropped and
/// `git apply` could not unlink the nested path.
#[test]
fn test_cli_diff_patch_nested_deleted_file_round_trips() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::create_dir_all(temp.path().join("src/nested")).unwrap();
    std::fs::write(temp.path().join("src/nested/file.txt"), "alpha\nbeta\n").unwrap();
    std::fs::write(temp.path().join("keep.txt"), "keep\n").unwrap();
    heddle(&["capture", "-m", "v1"], Some(temp.path())).unwrap();
    std::fs::remove_file(temp.path().join("src/nested/file.txt")).unwrap();

    let patch = heddle(&["diff", "--patch"], Some(temp.path())).unwrap();
    assert!(
        patch.contains("deleted file mode 100644"),
        "nested delete must carry the `deleted file mode` header:\n{patch}"
    );
    assert!(
        patch.contains("--- a/src/nested/file.txt") && patch.contains("+++ /dev/null"),
        "nested delete must source the new side from `/dev/null`:\n{patch}"
    );
    assert!(
        patch.contains("-alpha") && patch.contains("-beta"),
        "nested delete body must include every removed line:\n{patch}"
    );

    let apply_dir = TempDir::new().unwrap();
    init_git_repo(apply_dir.path());
    std::fs::create_dir_all(apply_dir.path().join("src/nested")).unwrap();
    std::fs::write(
        apply_dir.path().join("src/nested/file.txt"),
        "alpha\nbeta\n",
    )
    .unwrap();
    std::fs::write(apply_dir.path().join("keep.txt"), "keep\n").unwrap();
    git_commit_all(apply_dir.path(), "seed");
    git_apply(apply_dir.path(), &patch);
    assert!(
        !apply_dir.path().join("src/nested/file.txt").exists(),
        "applying the nested-delete patch must unlink the nested file:\n{patch}"
    );
}

/// Dropping only the trailing newline (`hello\n` -> `hello`) is a real
/// change even though every line is shared context after the diff
/// backend strips terminators. The renderer must synthesize a tail hunk
/// and attach `\ No newline at end of file` so the edit round-trips.
#[test]
fn test_cli_diff_patch_newline_only_removal_round_trips() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("nl.txt"), "hello\n").unwrap();
    heddle(&["capture", "-m", "v1"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("nl.txt"), "hello").unwrap();

    let patch = heddle(&["diff", "--patch"], Some(temp.path())).unwrap();
    assert!(
        patch.contains("@@"),
        "newline-only removal must emit a hunk:\n{patch}"
    );
    assert!(
        patch.contains("\\ No newline at end of file"),
        "newline-only removal must carry the no-newline marker:\n{patch}"
    );

    let apply_dir = TempDir::new().unwrap();
    init_git_repo(apply_dir.path());
    std::fs::write(apply_dir.path().join("nl.txt"), "hello\n").unwrap();
    git_commit_all(apply_dir.path(), "seed");
    git_apply(apply_dir.path(), &patch);
    assert_eq!(
        std::fs::read(apply_dir.path().join("nl.txt")).unwrap(),
        b"hello",
        "applying the patch must drop the trailing newline"
    );
}

/// Mirror of the removal: adding a trailing newline (`hello` ->
/// `hello\n`) must also synthesize a tail hunk, with the marker on the
/// old side.
#[test]
fn test_cli_diff_patch_newline_only_addition_round_trips() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("nl.txt"), "hello").unwrap();
    heddle(&["capture", "-m", "v1"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("nl.txt"), "hello\n").unwrap();

    let patch = heddle(&["diff", "--patch"], Some(temp.path())).unwrap();
    assert!(
        patch.contains("@@") && patch.contains("\\ No newline at end of file"),
        "newline-only addition must emit a hunk with the no-newline marker:\n{patch}"
    );

    let apply_dir = TempDir::new().unwrap();
    init_git_repo(apply_dir.path());
    std::fs::write(apply_dir.path().join("nl.txt"), "hello").unwrap();
    git_commit_all(apply_dir.path(), "seed");
    git_apply(apply_dir.path(), &patch);
    assert_eq!(
        std::fs::read(apply_dir.path().join("nl.txt")).unwrap(),
        b"hello\n",
        "applying the patch must add the trailing newline"
    );
}

/// `heddle --output json diff` (no `--patch`) on a dirty plain-Git
/// checkout must still populate the top-level `patch` field. The fast
/// path used to gate hunk inflation on the CLI `--patch` flag alone, so
/// JSON consumers got no parseable patch unless they also passed
/// `--patch`.
#[test]
fn test_cli_diff_json_plain_git_patch_field_present_without_flag() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("file.txt"), "a\nb\nc\n").unwrap();
    git_commit_all(temp.path(), "seed");
    std::fs::write(temp.path().join("file.txt"), "a\nB\nc\n").unwrap();

    let output = heddle_output(&["--output", "json", "diff"], Some(temp.path()))
        .expect("heddle diff should run");
    assert!(
        output.status.success(),
        "heddle diff should succeed; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value = json_stdout(&output, "plain-git json no flag");
    let patch_field = value
        .get("patch")
        .and_then(Value::as_str)
        .expect("JSON output must include `patch` field even without --patch");
    assert!(
        patch_field.contains("--- a/file.txt")
            && patch_field.contains("-b")
            && patch_field.contains("+B"),
        "patch field must carry the unified-diff body: {patch_field}"
    );
}

/// The trust-visible Heddle fast path (a git-adopted repo whose branch
/// advanced outside Heddle) must populate the JSON `patch` field too,
/// without `--patch`. Same contract as the plain-Git path.
#[test]
fn test_cli_diff_json_trust_visible_patch_field_present_without_flag() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("file.txt"), "a\nb\nc\n").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle_adopt(temp.path());
    // Advance the git branch outside Heddle so the verification state is
    // `git_branch_advanced`, which routes `diff` through the
    // trust-visible worktree-status fast path.
    std::fs::write(temp.path().join("file.txt"), "a\nb\nc\nd\n").unwrap();
    git(&["add", "file.txt"], temp.path());
    git(&["commit", "-m", "manual git commit"], temp.path());
    // An uncommitted worktree edit on top, so the diff has a body.
    std::fs::write(temp.path().join("file.txt"), "a\nB\nc\nd\n").unwrap();

    let output = heddle_output(&["--output", "json", "diff"], Some(temp.path()))
        .expect("heddle diff should run");
    assert!(
        output.status.success(),
        "heddle diff should succeed; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value = json_stdout(&output, "trust-visible json no flag");
    let patch_field = value
        .get("patch")
        .and_then(Value::as_str)
        .expect("trust-visible JSON output must include `patch` field even without --patch");
    assert!(
        patch_field.contains("--- a/file.txt") && patch_field.contains("+++ b/file.txt"),
        "patch field must carry the standard headers: {patch_field}"
    );
}

/// A state-to-state added file (`heddle diff <from> <to>`) must carry
/// the real `new file mode` from the destination tree entry — exercises
/// the `to_tree` branch of the mode resolver, distinct from the
/// worktree-add path.
#[test]
fn test_cli_diff_patch_state_to_state_add_carries_mode() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "v1"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("fresh.txt"), "fresh\n").unwrap();
    heddle(&["capture", "-m", "v2"], Some(temp.path())).unwrap();

    let patch = heddle(&["diff", "HEAD~1", "HEAD", "--patch"], Some(temp.path())).unwrap();
    assert!(
        patch.contains("diff --git a/fresh.txt b/fresh.txt")
            && patch.contains("new file mode 100644"),
        "state-to-state add must carry the `new file mode` header:\n{patch}"
    );
    assert!(
        patch.contains("+fresh"),
        "state-to-state add body must include the new content:\n{patch}"
    );
}

/// Deleting a committed executable and symlink on the plain-Git fast
/// path must carry their real modes (`100755` / `120000`), read from the
/// gix HEAD tree entry, and round-trip through `git apply`.
#[cfg(unix)]
#[test]
fn test_cli_diff_patch_plain_git_delete_executable_and_symlink_modes() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::os::unix::fs::symlink("the/target", temp.path().join("oldlink")).unwrap();
    let script = temp.path().join("old.sh");
    std::fs::write(&script, "#!/bin/sh\necho x\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
    }
    std::fs::write(temp.path().join("keep.txt"), "keep\n").unwrap();
    git_commit_all(temp.path(), "seed");
    std::fs::remove_file(temp.path().join("oldlink")).unwrap();
    std::fs::remove_file(&script).unwrap();

    let patch = heddle(&["diff", "--patch"], Some(temp.path())).unwrap();
    assert!(
        patch.contains("deleted file mode 100755"),
        "deleted executable must carry mode 100755:\n{patch}"
    );
    assert!(
        patch.contains("deleted file mode 120000"),
        "deleted symlink must carry mode 120000:\n{patch}"
    );

    let apply_dir = TempDir::new().unwrap();
    init_git_repo(apply_dir.path());
    std::os::unix::fs::symlink("the/target", apply_dir.path().join("oldlink")).unwrap();
    let apply_script = apply_dir.path().join("old.sh");
    std::fs::write(&apply_script, "#!/bin/sh\necho x\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&apply_script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&apply_script, perms).unwrap();
    }
    std::fs::write(apply_dir.path().join("keep.txt"), "keep\n").unwrap();
    git_commit_all(apply_dir.path(), "seed");
    git_apply(apply_dir.path(), &patch);
    assert!(
        !apply_dir.path().join("oldlink").exists() && !apply_dir.path().join("old.sh").exists(),
        "applying the delete patch must unlink both special files:\n{patch}"
    );
}

/// heddle#464 close-the-class: `heddle start` validates the thread name at the
/// creation boundary. A name with a space (or any shell metacharacter) is
/// rejected with the centralized `thread_name_invalid` advice — in BOTH text
/// and JSON-error modes — and never persisted, so no downstream breadcrumb can
/// interpolate an unsafe thread id. The CLI exits non-zero; it does not panic.
#[test]
fn start_rejects_thread_name_with_space_in_text_and_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    // Text mode: non-zero exit, actionable rename hint on stderr.
    let text = heddle_output(&["start", "my feature"], Some(temp.path())).unwrap();
    assert!(
        !text.status.success(),
        "an invalid thread name must be rejected: stdout={}",
        String::from_utf8_lossy(&text.stdout)
    );
    let text_stderr = String::from_utf8_lossy(&text.stderr);
    assert!(
        text_stderr.contains("is invalid"),
        "text mode must explain the name is invalid: {text_stderr}"
    );
    assert!(
        text_stderr.contains("try 'my-feature'"),
        "the rename hint must suggest a valid name: {text_stderr}"
    );

    // JSON mode: still rejected with the same kind, no panic.
    let json_out = heddle_output(
        &["start", "my feature", "--output", "json"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        !json_out.status.success(),
        "an invalid thread name must be rejected in JSON mode too"
    );
    let json_stderr = String::from_utf8_lossy(&json_out.stderr);
    assert!(
        json_stderr.contains("thread_name_invalid"),
        "JSON-error mode must carry the thread_name_invalid kind: {json_stderr}"
    );

    // The invalid name was rejected before any thread was created.
    let list = heddle(&["thread", "list", "--output", "json"], Some(temp.path())).unwrap();
    assert!(
        !list.contains("my feature"),
        "the rejected name must never have been persisted: {list}"
    );
}

/// heddle#464 close-the-class (round 6): the SAME early-reject rule must guard
/// every user/external thread-creation boundary, not just `start`. `heddle
/// thread create` with a shell-metacharacter name is rejected before any ref or
/// record is persisted.
#[test]
fn thread_create_rejects_thread_name_with_metachar() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let out = heddle_output(&["thread", "create", "bad;id"], Some(temp.path())).unwrap();
    assert!(
        !out.status.success(),
        "thread create must reject an unsafe name: stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("is invalid"),
        "thread create must explain the name is invalid: {stderr}"
    );

    let list = heddle(&["thread", "list", "--output", "json"], Some(temp.path())).unwrap();
    assert!(
        !list.contains("bad;id"),
        "the rejected name must never have been persisted: {list}"
    );
}

/// heddle#464 close-the-class: `thread rename` writes a NEW thread id, so the
/// destination name is a creation boundary too — an unsafe new name is rejected
/// and the original thread is left untouched.
#[test]
fn thread_rename_rejects_unsafe_new_name() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(&["thread", "create", "safe-thread"], Some(temp.path())).unwrap();

    let out = heddle_output(
        &["thread", "rename", "safe-thread", "bad;id"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        !out.status.success(),
        "rename to an unsafe name must be rejected: stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("is invalid"),
        "rename must explain the name is invalid: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let list = heddle(&["thread", "list", "--output", "json"], Some(temp.path())).unwrap();
    assert!(
        !list.contains("bad;id"),
        "the rejected name must never have been persisted: {list}"
    );
    assert!(
        list.contains("safe-thread"),
        "a rejected rename must leave the original thread intact: {list}"
    );
}

/// heddle#464 close-the-class: `actor spawn --thread <name>` mints/attaches the
/// named thread, so a user-supplied unsafe name is rejected before any ref is
/// written. (The generated `actor/<session>` fallback is safe by construction.)
#[test]
fn actor_spawn_rejects_unsafe_thread_name() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let out = heddle_output(&["actor", "spawn", "--thread", "bad;id"], Some(temp.path())).unwrap();
    assert!(
        !out.status.success(),
        "actor spawn with an unsafe thread name must be rejected: stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("is invalid"),
        "actor spawn must explain the name is invalid: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let list = heddle(&["thread", "list", "--output", "json"], Some(temp.path())).unwrap();
    assert!(
        !list.contains("bad;id"),
        "the rejected name must never have been persisted: {list}"
    );
}

/// heddle#464 close-the-class (round 6): `heddle agent reserve` also persists a
/// thread record, so it must reject an unsafe thread name at the same boundary.
#[test]
fn agent_reserve_rejects_thread_name_with_metachar() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let out = heddle_output(
        &["agent", "reserve", "--thread", "bad;id"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        !out.status.success(),
        "agent reserve must reject an unsafe name: stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("is invalid"),
        "agent reserve must explain the name is invalid: {stderr}"
    );

    let list = heddle(&["thread", "list", "--output", "json"], Some(temp.path())).unwrap();
    assert!(
        !list.contains("bad;id"),
        "the rejected name must never have been persisted: {list}"
    );
}
