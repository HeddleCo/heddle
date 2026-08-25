use std::{
    fs,
    path::Path,
    process::{Command, Output, Stdio},
};

use tempfile::TempDir;

fn isolated_command(repo: &Path, home: &Path, heddle_home: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_heddle"));
    command
        .args(args)
        .current_dir(repo)
        .env("HOME", home)
        .env("HEDDLE_HOME", heddle_home)
        .env_remove("HEDDLE_CONFIG")
        .env_remove("HEDDLE_PRINCIPAL_NAME")
        .env_remove("HEDDLE_PRINCIPAL_EMAIL")
        .env_remove("HEDDLE_AGENT_PROVIDER")
        .env_remove("HEDDLE_AGENT_MODEL")
        // Plain-text assertions below require uncolored output. The host may
        // export CLICOLOR_FORCE=1 (which wins after NO_COLOR is cleared), so
        // force color off rather than inheriting ambient color policy.
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("FORCE_COLOR");
    command
}

fn output_text(output: &Output) -> String {
    String::from_utf8_lossy(&[output.stdout.as_slice(), output.stderr.as_slice()].concat())
        .into_owned()
}

fn assert_success(output: &Output, command: &str) {
    assert!(
        output.status.success(),
        "{command} failed with {:?}:\n{}",
        output.status.code(),
        output_text(output)
    );
}

#[test]
fn heddle_home_is_honored_alongside_heddle_config() {
    let temp = TempDir::new().expect("tempdir");
    let shared_home = temp.path().join("shared-home");
    let repo = temp.path().join("repo");
    let heddle_home = temp.path().join("myhome");
    let config = temp.path().join("my.toml");
    fs::create_dir_all(&shared_home).expect("shared home");
    fs::create_dir_all(&repo).expect("repo");
    super::git_hermetic(&["init"], &repo);

    let output = isolated_command(
        &repo,
        &shared_home,
        &heddle_home,
        &[
            "init",
            "--principal-name",
            "T",
            "--principal-email",
            "t@x.io",
        ],
    )
    .env("HEDDLE_CONFIG", &config)
    .output()
    .expect("run init");
    assert_success(&output, "init with HEDDLE_HOME and HEDDLE_CONFIG");
    assert!(config.is_file(), "HEDDLE_CONFIG should be written");
    assert!(
        heddle_home.is_dir(),
        "HEDDLE_HOME should be created instead of silently ignored"
    );
}

#[test]
fn unusable_heddle_home_fails_loudly_without_fallback() {
    let temp = TempDir::new().expect("tempdir");
    let shared_home = temp.path().join("shared-home");
    let repo = temp.path().join("repo");
    let heddle_home = temp.path().join("not-a-directory");
    let config = temp.path().join("my.toml");
    fs::create_dir_all(&shared_home).expect("shared home");
    fs::create_dir_all(&repo).expect("repo");
    fs::write(&heddle_home, "blocking file\n").expect("blocking file");
    super::git_hermetic(&["init"], &repo);

    let output = isolated_command(
        &repo,
        &shared_home,
        &heddle_home,
        &[
            "init",
            "--principal-name",
            "T",
            "--principal-email",
            "t@x.io",
        ],
    )
    .env("HEDDLE_CONFIG", &config)
    .output()
    .expect("run init");
    assert!(!output.status.success(), "unusable HEDDLE_HOME must fail");
    assert!(
        output_text(&output).contains(&heddle_home.display().to_string()),
        "error must identify the HEDDLE_HOME path:\n{}",
        output_text(&output)
    );
    assert!(!config.exists(), "must not fall back to HEDDLE_CONFIG only");
    assert!(
        !shared_home.join(".heddle").exists(),
        "must not write global state to the shared HOME"
    );
}

#[test]
fn heddle_home_isolates_concurrent_user_configs() {
    let temp = TempDir::new().expect("tempdir");
    let shared_home = temp.path().join("shared-home");
    let repo_a = temp.path().join("repo-a");
    let repo_b = temp.path().join("repo-b");
    let home_a = temp.path().join("heddle-a");
    let home_b = temp.path().join("heddle-b");
    fs::create_dir_all(&shared_home).expect("shared home");
    fs::create_dir_all(&repo_a).expect("repo a");
    fs::create_dir_all(&repo_b).expect("repo b");
    super::git_hermetic(&["init"], &repo_a);
    super::git_hermetic(&["init"], &repo_b);

    let child_a = isolated_command(
        &repo_a,
        &shared_home,
        &home_a,
        &[
            "init",
            "--principal-name",
            "Agent A",
            "--principal-email",
            "a@example.test",
        ],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn init a");
    let child_b = isolated_command(
        &repo_b,
        &shared_home,
        &home_b,
        &[
            "init",
            "--principal-name",
            "Agent B",
            "--principal-email",
            "b@example.test",
        ],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn init b");

    let output_a = child_a.wait_with_output().expect("wait init a");
    let output_b = child_b.wait_with_output().expect("wait init b");
    assert_success(&output_a, "init a");
    assert_success(&output_b, "init b");

    let config_a = fs::read_to_string(home_a.join("config.toml"))
        .expect("HEDDLE_HOME A should contain its user config");
    let config_b = fs::read_to_string(home_b.join("config.toml"))
        .expect("HEDDLE_HOME B should contain its user config");
    assert!(config_a.contains("Agent A") && !config_a.contains("Agent B"));
    assert!(config_b.contains("Agent B") && !config_b.contains("Agent A"));
    assert!(
        !shared_home.join(".config/heddle/config.toml").exists(),
        "HEDDLE_HOME must prevent fallback to the shared HOME"
    );

    let native_a = temp.path().join("native-a");
    let native_b = temp.path().join("native-b");
    // Git first, then bootstrap the Heddle sidecar: the
    // CheckpointAfterCapture tip (which writes the session marker under
    // HEDDLE_HOME) fires in Git Overlay captures only.
    fs::create_dir_all(&native_a).expect("repo a directory");
    fs::create_dir_all(&native_b).expect("repo b directory");
    super::git_hermetic(&["init"], &native_a);
    super::git_hermetic(&["init"], &native_b);
    repo::Repository::bootstrap_git_overlay(&native_a).expect("overlay repo a");
    repo::Repository::bootstrap_git_overlay(&native_b).expect("overlay repo b");
    fs::write(native_a.join("a.txt"), "a\n").expect("worktree a");
    fs::write(native_b.join("b.txt"), "b\n").expect("worktree b");

    let capture_a = isolated_command(
        &native_a,
        &shared_home,
        &home_a,
        &["--output", "json", "capture", "-m", "isolated capture a"],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn capture a");
    let capture_b = isolated_command(
        &native_b,
        &shared_home,
        &home_b,
        &["--output", "json", "capture", "-m", "isolated capture b"],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn capture b");
    let capture_output_a = capture_a.wait_with_output().expect("wait capture a");
    let capture_output_b = capture_b.wait_with_output().expect("wait capture b");
    assert_success(&capture_output_a, "capture a");
    assert_success(&capture_output_b, "capture b");
    let capture_json_a: serde_json::Value =
        serde_json::from_slice(&capture_output_a.stdout).expect("capture a JSON");
    let capture_json_b: serde_json::Value =
        serde_json::from_slice(&capture_output_b.stdout).expect("capture b JSON");
    assert_eq!(capture_json_a["principal"]["name"], "Agent A");
    assert_eq!(capture_json_a["principal"]["email"], "a@example.test");
    assert_eq!(capture_json_a["principal_source"], "user_config");
    assert_eq!(capture_json_b["principal"]["name"], "Agent B");
    assert_eq!(capture_json_b["principal"]["email"], "b@example.test");
    assert_eq!(capture_json_b["principal_source"], "user_config");
    assert!(
        !home_a.join("session").exists() && !home_b.join("session").exists(),
        "native capture is the save boundary and must not write the Git Overlay checkpoint-tip marker"
    );
    assert!(
        !shared_home.join(".heddle/session").exists(),
        "native capture must not write tip session state under the shared HOME"
    );
}

#[test]
fn init_status_and_capture_agree_on_user_config_principal() {
    let temp = TempDir::new().expect("tempdir");
    let shared_home = temp.path().join("shared-home");
    let heddle_home = temp.path().join("isolated-heddle-home");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo");
    super::git_hermetic(&["init"], &repo);
    fs::create_dir_all(shared_home.join(".config/heddle")).expect("shared config directory");
    fs::write(
        shared_home.join(".config/heddle/config.toml"),
        "[principal]\nname = \"Stale Stranger\"\nemail = \"stale@example.invalid\"\n",
    )
    .expect("stale shared config");

    let init = isolated_command(
        &repo,
        &shared_home,
        &heddle_home,
        &[
            "init",
            "--principal-name",
            "Luke Thorne",
            "--principal-email",
            "luke@example.test",
        ],
    )
    .output()
    .expect("run init");
    assert_success(&init, "init");
    assert!(
        output_text(&init).contains(
            "Principal: Luke Thorne <luke@example.test> from user_config (shared global config)"
        ),
        "init must report the resolved principal and its global source:\n{}",
        output_text(&init)
    );

    let status = isolated_command(&repo, &shared_home, &heddle_home, &["status"])
        .output()
        .expect("run status");
    assert_success(&status, "status");
    assert!(
        output_text(&status).contains(
            "Identity: Luke Thorne <luke@example.test> from user_config (shared global config)"
        ),
        "status must report the same principal and source:\n{}",
        output_text(&status)
    );

    fs::write(repo.join("identity.txt"), "identity evidence\n").expect("worktree change");
    let capture = isolated_command(
        &repo,
        &shared_home,
        &heddle_home,
        &["capture", "-m", "identity resolution evidence"],
    )
    .output()
    .expect("run capture");
    assert_success(&capture, "capture");
    assert!(
        output_text(&capture).contains(
            "Captured by: Luke Thorne <luke@example.test> from user_config (shared global config)"
        ),
        "capture must report the same principal and source:\n{}",
        output_text(&capture)
    );

    fs::write(repo.join("identity-json.txt"), "machine evidence\n").expect("second change");
    let json_capture = isolated_command(
        &repo,
        &shared_home,
        &heddle_home,
        &["capture", "-m", "machine evidence", "--output", "json"],
    )
    .output()
    .expect("JSON capture");
    assert_success(&json_capture, "JSON capture");
    let json: serde_json::Value =
        serde_json::from_slice(&json_capture.stdout).expect("capture JSON");
    assert_eq!(json["principal_source"], "user_config");

    let isolated_config =
        fs::read_to_string(heddle_home.join("config.toml")).expect("isolated user config");
    assert!(isolated_config.contains("Luke Thorne"));
    let stale_config = fs::read_to_string(shared_home.join(".config/heddle/config.toml"))
        .expect("shared config remains readable");
    assert!(
        stale_config.contains("Stale Stranger"),
        "the isolated run must not overwrite shared global config"
    );
}
