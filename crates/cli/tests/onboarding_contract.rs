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

fn snapshot_outside_repository(
    root: &Path,
    repository: &Path,
) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    fn visit(
        root: &Path,
        directory: &Path,
        repository: &Path,
        snapshot: &mut Vec<(std::path::PathBuf, Vec<u8>)>,
    ) {
        let mut entries = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        entries.sort();
        for path in entries {
            if path == repository {
                continue;
            }
            if path.is_dir() {
                visit(root, &path, repository, snapshot);
            } else if path.is_file() {
                snapshot.push((
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(path).unwrap(),
                ));
            }
        }
    }

    let mut snapshot = Vec::new();
    visit(root, root, repository, &mut snapshot);
    snapshot
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
fn init_in_fresh_directory_does_not_write_to_ancestor_git_repository() {
    let fixture = TempDir::new().unwrap();
    let outer = fixture.path().join("outer");
    let repo = outer.join("fresh/repo");
    let config = repo.join("user/config.toml");
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(&outer).unwrap();
    init_git(&outer);
    let outside_before = snapshot_outside_repository(&outer, &repo);

    let init = json(&heddle(&repo, &config, &["init", "--output", "json"]));

    assert_eq!(init["repository_mode"], "native-heddle");
    assert_eq!(init["git_detected"], false);
    assert_eq!(
        snapshot_outside_repository(&outer, &repo),
        outside_before,
        "init must not write anywhere outside the requested repository root"
    );
    assert!(!outer.join(".heddle").exists());
    assert!(repo.join(".heddle").is_dir());
}

#[test]
fn cwd_native_repository_wins_over_readable_ancestor_git_repository() {
    let fixture = TempDir::new().unwrap();
    let outer = fixture.path().join("outer");
    let repo = outer.join("native");
    let config = repo.join("user/config.toml");
    fs::create_dir_all(&repo).unwrap();
    init_git(&outer);
    repo::Repository::init_default(&repo).unwrap();

    let status = json(&heddle(&repo, &config, &["status", "--output", "json"]));

    assert_eq!(status["repository_capability"], "native-heddle");
    assert_eq!(status["storage_model"], "heddle-native");
}

#[cfg(unix)]
#[test]
fn unreadable_ancestor_git_worktree_entry_is_non_fatal_for_native_status() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = TempDir::new().unwrap();
    let outer = fixture.path().join("outer");
    let repo = outer.join("native");
    let unreadable = outer.join("aaa-unreadable");
    let config = repo.join("user/config.toml");
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(&unreadable).unwrap();
    fs::write(unreadable.join("secret"), "not part of the native repo").unwrap();
    init_git(&outer);
    repo::Repository::init_default(&repo).unwrap();
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();

    let output = heddle(&repo, &config, &["status", "--output", "json"]);

    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o755)).unwrap();
    let status = json(&output);
    assert_eq!(status["repository_capability"], "native-heddle");
    assert_eq!(status["storage_model"], "heddle-native");
}

#[cfg(unix)]
#[test]
fn local_git_metadata_io_error_names_the_path() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = TempDir::new().unwrap();
    let repo = fixture.path().join("repo");
    let git_file = repo.join(".git");
    let config = repo.join("user/config.toml");
    fs::create_dir_all(&repo).unwrap();
    fs::write(&git_file, "gitdir: /unreadable\n").unwrap();
    fs::set_permissions(&git_file, fs::Permissions::from_mode(0o000)).unwrap();

    let output = heddle(&repo, &config, &["init", "--output", "json"]);

    fs::set_permissions(&git_file, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&git_file.display().to_string()),
        "I/O error must name the failing path: {stderr}"
    );
    assert!(!repo.join(".heddle").exists());
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

#[test]
fn adopt_moves_authority_to_native_and_retains_git_projection() {
    let repo = TempDir::new().unwrap();
    let config = repo.path().join("user/config.toml");
    init_git(repo.path());
    commit_git(repo.path());

    let adopted = json(&heddle(
        repo.path(),
        &config,
        &["adopt", "--output", "json"],
    ));
    assert_eq!(adopted["verification"]["repository_mode"], "native-heddle");

    let status = json(&heddle(
        repo.path(),
        &config,
        &["status", "--output", "json"],
    ));
    assert_eq!(status["repository_capability"], "native-heddle");
    assert_eq!(status["storage_model"], "heddle-native");
    assert!(repo.path().join(".git").is_dir());

    let imported = json(&heddle(
        repo.path(),
        &config,
        &["import", "git", "--ref", "main", "--output", "json"],
    ));
    assert_eq!(imported["output_kind"], "import_git");
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

#[cfg(unix)]
#[test]
fn repository_creation_failure_does_not_publish_principal_config() {
    use std::os::unix::fs::PermissionsExt;

    let root = TempDir::new().unwrap();
    let repo = root.path().join("readonly-repo");
    let config = root.path().join("user/config.toml");
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "").unwrap();
    fs::set_permissions(&repo, fs::Permissions::from_mode(0o555)).unwrap();

    let output = heddle(
        &repo,
        &config,
        &[
            "init",
            "--principal-name",
            "No Partial Write",
            "--principal-email",
            "atomic@example.com",
        ],
    );

    fs::set_permissions(&repo, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!output.status.success());
    assert_eq!(fs::read_to_string(&config).unwrap(), "");
    assert!(!repo.join(".heddle").exists());
}
