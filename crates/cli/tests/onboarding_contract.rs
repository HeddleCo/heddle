// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use serde_json::Value;
use tempfile::TempDir;

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("run git fixture command");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_git(repo: &Path) {
    git(repo, &["init", "-b", "main"]);
    git(repo, &["config", "user.name", "Onboarding Test"]);
    git(repo, &["config", "user.email", "onboarding@example.com"]);
}

fn commit_git(repo: &Path) {
    fs::write(repo.join("tracked.txt"), "tracked\n").unwrap();
    git(repo, &["add", "tracked.txt"]);
    git(repo, &["commit", "-m", "seed"]);
}

fn heddle(repo: &Path, config: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_heddle"))
        .args(args)
        .current_dir(repo)
        .env("HEDDLE_CONFIG", config)
        .env_remove("HEDDLE_PRINCIPAL_NAME")
        .env_remove("HEDDLE_PRINCIPAL_EMAIL")
        .output()
        .expect("run heddle")
}

fn json(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "heddle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("parse heddle JSON")
}

#[test]
fn committed_git_and_unborn_git_both_enter_through_init() {
    for committed in [false, true] {
        let repo = TempDir::new().unwrap();
        let config = repo.path().join("user/config.toml");
        init_git(repo.path());
        if committed {
            commit_git(repo.path());
        }

        let status = json(&heddle(
            repo.path(),
            &config,
            &["status", "--output", "json"],
        ));
        assert_eq!(status["repository_capability"], "plain-git");
        assert_eq!(status["recommended_action"], "heddle init");
        assert_eq!(status["verification"]["mapping_state"], "git_backed");
        let expected_state = if committed {
            "plain_git_committed"
        } else {
            "plain_git_unborn"
        };
        assert_eq!(
            status["verification"]["checks"][0]["details"]["onboarding_state"],
            expected_state
        );
        assert!(!repo.path().join(".heddle").exists());

        let init = json(&heddle(repo.path(), &config, &["init", "--output", "json"]));
        assert_eq!(init["repository_mode"], "git-overlay");
        assert_eq!(init["git_detected"], true);
        assert!(repo.path().join(".git").is_dir());
    }
}

#[test]
fn native_empty_directory_initializes_native_storage() {
    let repo = TempDir::new().unwrap();
    let config = repo.path().join("user/config.toml");
    let init = json(&heddle(repo.path(), &config, &["init", "--output", "json"]));

    assert_eq!(init["repository_mode"], "native-heddle");
    assert_eq!(init["git_detected"], false);
    assert!(repo.path().join(".heddle").is_dir());
    assert!(!repo.path().join(".git").exists());
}

#[test]
fn initialized_git_overlay_keeps_git_as_the_source_store() {
    let repo = TempDir::new().unwrap();
    let config = repo.path().join("user/config.toml");
    init_git(repo.path());
    commit_git(repo.path());

    json(&heddle(repo.path(), &config, &["init", "--output", "json"]));
    let status = json(&heddle(
        repo.path(),
        &config,
        &["status", "--output", "json"],
    ));

    assert_eq!(status["repository_capability"], "git-overlay");
    assert_eq!(status["storage_model"], "git+heddle-sidecar");
    assert_eq!(status["verification"]["mapping_state"], "git_backed");
    assert!(status["recommended_action"].is_null());
}

#[cfg(unix)]
#[test]
fn read_only_principal_config_refuses_before_repository_creation() {
    use std::os::unix::fs::PermissionsExt;

    let root = TempDir::new().unwrap();
    let repo = root.path().join("repo");
    let config_dir = root.path().join("readonly-config");
    let config = config_dir.join("config.toml");
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(&config, "").unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o444)).unwrap();
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o555)).unwrap();

    let output = heddle(
        &repo,
        &config,
        &[
            "init",
            "--principal-name",
            "Read Only",
            "--principal-email",
            "readonly@example.com",
        ],
    );

    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!output.status.success());
    assert_eq!(fs::read_to_string(&config).unwrap(), "");
    assert!(!repo.join(".heddle").exists());
}
