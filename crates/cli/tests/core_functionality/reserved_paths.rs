// SPDX-License-Identifier: Apache-2.0
//! Reserved metadata paths end to end (heddle#2028).
//!
//! Every command that writes tree content into a worktree must leave a
//! `.git` alias at any depth, a root `.heddle` alias and a `.gitmodules`
//! symlink alone. The hostile states here are planted through the library,
//! as a peer or a repository captured before heddle#2028 would supply them;
//! Git import refuses them (see `ingest` tests).

use std::{collections::BTreeMap, path::Path};

use objects::{
    object::{Blob, StateId, Tree, TreeEntry},
    store::ObjectStore,
};

use super::*;

const PWNED: &str = "pwned = true\n";

fn output(args: &[&str], cwd: &Path) -> std::process::Output {
    cli_test_support::heddle_output(args, Some(cwd)).expect("spawn heddle")
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn init_with_readme(root: &Path) -> StateId {
    heddle_must_succeed(&["init"], root);
    cli_test_support::seed_test_repo_principal(root).unwrap();
    std::fs::write(root.join("README.md"), "readme\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], root);
    Repository::open(root).unwrap().head().unwrap().unwrap()
}

/// Build a tree from `(path, content)` leaves, storing every blob and
/// subtree.
fn build_tree(repo: &Repository, files: &[(&str, &str)]) -> Tree {
    fn build(repo: &Repository, files: &[(Vec<&str>, &str)]) -> Tree {
        let mut dirs: BTreeMap<&str, Vec<(Vec<&str>, &str)>> = BTreeMap::new();
        let mut entries = Vec::new();
        for (components, content) in files {
            match components.as_slice() {
                [name] => {
                    let hash = repo.store().put_blob(&Blob::from_slice(content.as_bytes()));
                    entries.push(TreeEntry::file(*name, hash.unwrap(), false).unwrap());
                }
                [dir, rest @ ..] => dirs.entry(dir).or_default().push((rest.to_vec(), content)),
                [] => unreachable!("empty path"),
            }
        }
        for (dir, children) in dirs {
            let subtree = build(repo, &children);
            let hash = repo.store().put_tree(&subtree).unwrap();
            entries.push(TreeEntry::directory(dir, hash).unwrap());
        }
        Tree::from_entries(entries)
    }
    let files: Vec<_> = files
        .iter()
        .map(|(path, content)| (path.split('/').collect::<Vec<_>>(), *content))
        .collect();
    let tree = build(repo, &files);
    repo.store().put_tree(&tree).unwrap();
    tree
}

/// Capture `files` as the next state of the checkout at `root`, without
/// touching the worktree.
fn plant(root: &Path, files: &[(&str, &str)]) -> (StateId, Tree) {
    let repo = Repository::open(root).unwrap();
    let tree = build_tree(&repo, files);
    let attribution = repo.get_attribution().unwrap();
    let execution = repo
        .snapshot_tree_with_attribution_profiled(
            tree.clone(),
            Some("planted".to_string()),
            None,
            attribution,
        )
        .unwrap();
    // Keep the thread record in step, as a CLI capture does.
    repo::refresh_active_thread_metadata(&repo, &execution.state, &tree).unwrap();
    (execution.state.id(), tree)
}

fn config(root: &Path) -> String {
    std::fs::read_to_string(root.join(".heddle/config.toml")).unwrap()
}

/// A repository captured before heddle#2028 holds a vendored clone's
/// `.git`. It still logs, checks out (skipping the nested metadata with a
/// warning), reports clean, and can be left; capture skips a nested `.git`
/// with a warning naming it.
#[test]
fn a_captured_nested_git_logs_checks_out_and_can_be_left() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    init_with_readme(root);
    heddle_must_succeed(&["thread", "create", "before-vendor"], root);

    // The user's own clone, which owns `vendor/lib/.git`.
    std::fs::create_dir_all(root.join("vendor/lib/.git")).unwrap();
    std::fs::write(root.join("vendor/lib/.git/config"), "real\n").unwrap();
    std::fs::write(root.join("vendor/lib/src.rs"), "fn v1() {}\n").unwrap();
    let captured = output(&["capture", "-m", "vendor"], root);
    assert!(captured.status.success(), "{}", stderr(&captured));
    assert!(
        stderr(&captured).contains("vendor/lib/.git"),
        "capture must warn that it skips the nested .git: {}",
        stderr(&captured)
    );

    // Now the history an older Heddle wrote: the nested `.git` recorded.
    let (planted, _) = plant(
        root,
        &[
            ("README.md", "readme\n"),
            ("vendor/lib/src.rs", "fn v1() {}\n"),
            ("vendor/lib/.git/config", "[core]\n\thooksPath = /tmp/pwn\n"),
            (
                "vendor/lib/.git/hooks/post-checkout",
                "#!/bin/sh\necho pwned\n",
            ),
        ],
    );

    let log = output(&["--output", "json", "log", "-n", "1"], root);
    assert!(log.status.success(), "log: {}", stderr(&log));
    assert!(String::from_utf8_lossy(&log.stdout).contains(&planted.short()));

    let status = output(&["--output", "json", "status"], root);
    assert!(status.status.success(), "status: {}", stderr(&status));
    let status_json: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(
        status_json["changed_path_count"], 0,
        "a skipped nested .git is not a change: {status_json}"
    );
    assert!(
        stderr(&status).contains("vendor/lib/.git"),
        "status must warn that it skips the nested .git: {}",
        stderr(&status)
    );

    // Leave it: the tracked file goes, the clone's metadata stays.
    let left = output(&["thread", "switch", "before-vendor"], root);
    assert!(left.status.success(), "switch away: {}", stderr(&left));
    assert!(!root.join("vendor/lib/src.rs").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("vendor/lib/.git/config")).unwrap(),
        "real\n"
    );

    // Come back: the content is written, the nested metadata is not.
    let back = output(&["thread", "switch", "main"], root);
    assert!(back.status.success(), "switch back: {}", stderr(&back));
    assert_eq!(
        std::fs::read_to_string(root.join("vendor/lib/src.rs")).unwrap(),
        "fn v1() {}\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("vendor/lib/.git/config")).unwrap(),
        "real\n"
    );
    assert!(!root.join("vendor/lib/.git/hooks/post-checkout").exists());
}

/// `revert` of a state that deleted `.git` and `.heddle/config.toml` would
/// write the hostile parent's copies back: a gitfile pointing Git at another
/// repository, and the live config.
#[test]
fn revert_never_writes_metadata() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    init_with_readme(root);
    plant(
        root,
        &[
            ("README.md", "readme\n"),
            (".git", "gitdir: /tmp/attacker-repo\n"),
            (".heddle/config.toml", PWNED),
        ],
    );
    let (deleting, _) = plant(root, &[("README.md", "readme\n")]);
    let before = config(root);

    let reverted = output(&["revert", "--no-commit", &deleting.to_string()], root);
    assert!(
        fs_absent(&root.join(".git")),
        "revert wrote a .git gitfile: {}",
        stderr(&reverted)
    );
    assert_eq!(config(root), before, "revert rewrote .heddle/config.toml");
    assert!(reverted.status.success(), "revert: {}", stderr(&reverted));
    assert!(
        stderr(&reverted).contains("skipping '.git'"),
        "revert must warn about the skipped path: {}",
        stderr(&reverted)
    );
}

/// `resolve --theirs` on a conflict at `.heddle/config.toml` would write
/// their side into the live config.
#[test]
fn resolve_refuses_a_heddle_conflict_path() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let base = init_with_readme(root);
    let (theirs, _) = plant(
        root,
        &[("README.md", "readme\n"), (".heddle/config.toml", PWNED)],
    );
    let (ours, _) = plant(
        root,
        &[("README.md", "readme\n"), (".heddle/config.toml", "ours\n")],
    );
    Repository::open(root)
        .unwrap()
        .merge_state_manager()
        .start(
            ours,
            theirs,
            Some(base),
            vec![".heddle/config.toml".to_string()],
            None,
        )
        .unwrap();
    let before = config(root);

    let resolved = output(&["resolve", "--theirs", ".heddle/config.toml"], root);
    assert_eq!(config(root), before, "resolve rewrote .heddle/config.toml");
    assert!(!resolved.status.success(), "resolve must refuse the path");
    assert!(
        stderr(&resolved).contains(".heddle metadata directory"),
        "{}",
        stderr(&resolved)
    );
}

/// `thread move` copies the moved paths into the target checkout. History
/// recorded before heddle#2028 can carry a `.git` gitfile; moved into the
/// target it would point Git at a repository (and hooks) of the source's
/// choosing.
#[test]
fn thread_move_never_writes_a_git_gitfile() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    init_with_readme(root);
    let start = |name: &str| -> std::path::PathBuf {
        let started: Value = serde_json::from_str(&heddle_must_succeed(
            &["--output", "json", "start", name, "--workspace", "solid"],
            root,
        ))
        .unwrap();
        std::path::PathBuf::from(started["execution_path"].as_str().unwrap())
    };
    let source = start("feature/source");
    let target = start("feature/target");
    plant(
        &source,
        &[
            ("README.md", "readme\n"),
            (".git", "gitdir: /tmp/attacker-repo\n"),
        ],
    );

    let moved = output(
        &[
            "thread",
            "move",
            "feature/source",
            "feature/target",
            "--path",
            ".git",
        ],
        root,
    );
    assert!(
        fs_absent(&target.join(".git")),
        "thread move wrote a .git gitfile: {}",
        stderr(&moved)
    );
    assert!(
        stderr(&moved).contains("skipping '.git'"),
        "thread move must warn about the skipped path: {}",
        stderr(&moved)
    );
}

fn fs_absent(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_err()
}

/// Merge output drops entries the merged tree no longer has. A current
/// state holding `.heddle/config.toml` would make it delete the live config,
/// and a merged tree holding `.HEDDLE/…` would write through it.
#[test]
fn merge_output_never_touches_heddle() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    init_with_readme(root);
    plant(
        root,
        &[("README.md", "readme\n"), (".heddle/config.toml", PWNED)],
    );
    let before = config(root);
    let repo = Repository::open(root).unwrap();
    let merged = build_tree(
        &repo,
        &[
            ("README.md", "merged\n"),
            (".HEDDLE/hooks/pre-capture", "#!/bin/sh\necho pwned\n"),
        ],
    );

    verbs::apply_merged_tree(&repo, &merged).unwrap();
    assert_eq!(
        config(root),
        before,
        "merge output touched .heddle/config.toml"
    );
    assert!(!root.join(".HEDDLE").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("README.md")).unwrap(),
        "merged\n"
    );
}
