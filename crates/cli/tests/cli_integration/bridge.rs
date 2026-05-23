// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_cli_bridge_git_init() {
    let temp = TempDir::new().unwrap();

    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Initial"], Some(temp.path())).unwrap();

    assert!(heddle(&["bridge", "init"], Some(temp.path())).is_ok());
    assert!(
        temp.path().join(".heddle/git").exists(),
        "Git mirror should exist"
    );
}

#[test]
fn test_cli_bridge_git_export_and_pull_roundtrip() {
    let source = TempDir::new().unwrap();
    let target = TempDir::new().unwrap();
    let dest_holder = TempDir::new().unwrap();
    let dest = dest_holder.path().join("export");

    heddle(&["init"], Some(source.path())).unwrap();
    std::fs::write(source.path().join("file.txt"), "bridge export").unwrap();
    heddle(&["capture", "-m", "Bridge source"], Some(source.path())).unwrap();

    // Phase A: `bridge export` requires `--destination`. Pre-Phase-A
    // it silently no-op'd if no flag was given (writing only the sidecar
    // mapping, not actually exporting any git objects). Now it errors.
    let export = heddle(
        &["bridge", "export", "--destination", dest.to_str().unwrap()],
        Some(source.path()),
    );
    assert!(export.is_ok(), "bridge export failed: {:?}", export.err());

    let dest_repo = gix::open(&dest).unwrap();
    assert!(dest_repo.find_reference("refs/heads/main").is_ok());

    heddle(&["init"], Some(target.path())).unwrap();
    let pull = heddle(
        &["bridge", "pull", dest.to_str().unwrap()],
        Some(target.path()),
    );
    assert!(pull.is_ok(), "Bridge pull failed: {:?}", pull.err());

    let target_repo = Repository::open(target.path()).unwrap();
    assert!(target_repo.refs().get_thread("main").unwrap().is_some());
}

#[test]
fn test_cli_bridge_git_import_from_external_repo() {
    let heddle_repo_dir = TempDir::new().unwrap();
    let git_repo_dir = TempDir::new().unwrap();
    let git_repo = gix::init(git_repo_dir.path()).unwrap();
    let tree_oid = git_empty_tree_oid(&git_repo);
    git_commit_with_tree(
        &git_repo,
        Some("refs/heads/main"),
        tree_oid,
        "Imported commit",
        &[],
    );

    heddle(&["init"], Some(heddle_repo_dir.path())).unwrap();
    let result = heddle(
        &[
            "bridge",
            "import",
            "--path",
            git_repo_dir.path().to_str().unwrap(),
        ],
        Some(heddle_repo_dir.path()),
    );
    assert!(result.is_ok(), "Bridge import failed: {:?}", result.err());

    let repo = Repository::open(heddle_repo_dir.path()).unwrap();
    assert!(repo.refs().get_thread("main").unwrap().is_some());
}

#[test]
fn test_cli_bridge_git_push_to_local_bare_remote() {
    let source = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();
    let remote_repo = gix::init_bare(remote.path()).unwrap();

    heddle(&["init"], Some(source.path())).unwrap();
    std::fs::write(source.path().join("push.txt"), "bridge push").unwrap();
    heddle(&["capture", "-m", "Bridge push"], Some(source.path())).unwrap();

    let result = heddle(
        &["bridge", "push", remote.path().to_str().unwrap()],
        Some(source.path()),
    );
    assert!(result.is_ok(), "Bridge push failed: {:?}", result.err());

    assert!(remote_repo.find_reference("refs/heads/main").is_ok());
}
