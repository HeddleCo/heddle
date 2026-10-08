//! Non-canonical Git tree round-trip gate (heddle#2018).
//!
//! Real repositories contain Git trees that Git itself would never write:
//! zero-padded or odd file modes (`040000`, `100664`) and entries that are not
//! in Git's canonical order. rails/rails has 145 of them. Heddle must record
//! the raw mode and the source order so that git -> native -> git reproduces
//! every tree, and therefore every commit and every descendant commit, at its
//! original object id.
//!
//! The trees are hand-written with `git hash-object --literally` because no Git
//! porcelain will produce them, which also means `git fsck --strict` rejects
//! the fixture by design.

use std::{collections::BTreeMap, num::NonZeroUsize, path::Path, process::Command, sync::Arc};

use cli::Repository;
use heddle_git_projection::git_core::GitProjection;
use objects::store::{
    FsRepackOperation, RepackPolicy, RepackResourceLimits, RepackSchedule, RepackScheduler,
};
use tempfile::TempDir;

const ENV: &[(&str, &str)] = &[
    ("GIT_AUTHOR_NAME", "Heddle Conformance"),
    ("GIT_AUTHOR_EMAIL", "conformance@heddle.test"),
    ("GIT_COMMITTER_NAME", "Heddle Conformance"),
    ("GIT_COMMITTER_EMAIL", "conformance@heddle.test"),
    ("GIT_AUTHOR_DATE", "1700000000 +0000"),
    ("GIT_COMMITTER_DATE", "1700000000 +0000"),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("LC_ALL", "C"),
    ("TZ", "UTC"),
];

fn run_git(dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> String {
    use std::{io::Write, process::Stdio};

    let mut child = Command::new("git")
        .args(args)
        .current_dir(dir)
        .envs(ENV.iter().copied())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("failed to spawn git {args:?}: {error}"));
    if let Some(input) = stdin {
        child
            .stdin
            .as_mut()
            .expect("git stdin")
            .write_all(input)
            .expect("write git stdin");
    }
    let output = child.wait_with_output().expect("wait for git");
    assert!(
        output.status.success(),
        "git {args:?} failed in {}:\nstdout: {}\nstderr: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn git(dir: &Path, args: &[&str]) -> String {
    run_git(dir, args, None)
}

fn write_object(dir: &Path, kind: &str, body: &[u8]) -> String {
    run_git(
        dir,
        &["hash-object", "--literally", "-w", "-t", kind, "--stdin"],
        Some(body),
    )
}

fn blob(dir: &Path, content: &str) -> String {
    write_object(dir, "blob", content.as_bytes())
}

/// Write a tree with entries exactly as given: raw mode text, name, and order.
fn raw_tree(dir: &Path, entries: &[(&str, &str, &str)]) -> String {
    let mut body = Vec::new();
    for (mode, name, oid) in entries {
        body.extend_from_slice(mode.as_bytes());
        body.push(b' ');
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(&hex::decode(oid).expect("hex oid"));
    }
    write_object(dir, "tree", &body)
}

fn commit(dir: &Path, tree: &str, parent: Option<&str>, message: &str) -> String {
    let mut args = vec!["commit-tree", tree, "-m", message];
    if let Some(parent) = parent {
        args.extend(["-p", parent]);
    }
    git(dir, &args)
}

/// Build the corpus: one branch per non-canonical shape, each with a root
/// commit on the odd tree and a descendant commit that keeps it.
fn build_corpus(dir: &Path) -> BTreeMap<String, String> {
    git(dir, &["init", "-q", "--initial-branch=main"]);
    let a = blob(dir, "a\n");
    let b = blob(dir, "b\n");
    let c = blob(dir, "c\n");
    let leaf = raw_tree(dir, &[("100644", "x", &b)]);

    let mut branches = BTreeMap::new();

    // 100664: a group-writable regular file, a mode Git never writes.
    let odd_mode = raw_tree(dir, &[("100644", "a.txt", &a), ("100664", "b.txt", &b)]);
    let root = commit(dir, &odd_mode, None, "100664 root");
    // The descendant reuses the odd tree unchanged as a subtree.
    let child_tree = raw_tree(
        dir,
        &[("100644", "new.txt", &c), ("40000", "sub", &odd_mode)],
    );
    let child = commit(dir, &child_tree, Some(&root), "100664 descendant");
    branches.insert("mode-100664".to_string(), child);

    // 040000: a zero-padded directory mode.
    let padded = raw_tree(dir, &[("100644", "a.txt", &a), ("040000", "dir", &leaf)]);
    let root = commit(dir, &padded, None, "040000 root");
    // The descendant edits a sibling and keeps the padded entry.
    let padded_child = raw_tree(dir, &[("100644", "a.txt", &c), ("040000", "dir", &leaf)]);
    let child = commit(dir, &padded_child, Some(&root), "040000 descendant");
    branches.insert("mode-040000".to_string(), child);

    // Unsorted entries, including a tree that native byte order would put
    // first (`lib`) but Git's canonical order puts after `lib.rs`.
    let unsorted = raw_tree(
        dir,
        &[
            ("100644", "b.txt", &b),
            ("100644", "a.txt", &a),
            ("40000", "lib", &leaf),
            ("100644", "lib.rs", &c),
        ],
    );
    let root = commit(dir, &unsorted, None, "unsorted root");
    let unsorted_child = raw_tree(
        dir,
        &[
            ("100644", "b.txt", &b),
            ("100644", "a.txt", &a),
            ("100644", "c.txt", &c),
            ("40000", "lib", &leaf),
            ("100644", "lib.rs", &c),
        ],
    );
    let child = commit(dir, &unsorted_child, Some(&root), "unsorted descendant");
    branches.insert("unsorted".to_string(), child);

    for (branch, tip) in &branches {
        git(dir, &["update-ref", &format!("refs/heads/{branch}"), tip]);
    }
    // Guard against a fixture that silently became canonical.
    assert!(raw_tree_contains(dir, &odd_mode, b"100664 b.txt\0"));
    assert!(raw_tree_contains(dir, &padded, b"040000 dir\0"));
    assert!(raw_tree_contains(dir, &unsorted, b"b.txt\0"));
    branches
}

fn raw_tree_contains(dir: &Path, tree: &str, needle: &[u8]) -> bool {
    let output = Command::new("git")
        .args(["cat-file", "tree", tree])
        .current_dir(dir)
        .envs(ENV.iter().copied())
        .output()
        .expect("cat-file tree");
    output
        .stdout
        .windows(needle.len())
        .any(|window| window == needle)
}

fn every_commit_and_tree(dir: &Path) -> Vec<String> {
    let mut ids = git(dir, &["rev-list", "--objects", "--all"])
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_string)
        .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    ids
}

fn import(bridge: &mut GitProjection<'_>, repo: &Repository, source: &Path, direct: bool) {
    if direct {
        ingest::import_git_into_scoped_with_options(
            source,
            repo.root(),
            ingest::ImportOptions::default(),
            ingest::ImportScope::all(),
        )
        .unwrap_or_else(|error| panic!("direct ingest failed: {error}"));
        bridge
            .build_existing_mapping(Some(source))
            .unwrap_or_else(|error| panic!("mapping build failed: {error}"));
        bridge
            .seed_ingest_identity_mappings_from_store()
            .unwrap_or_else(|error| panic!("mapping seed failed: {error}"));
        bridge
            .save_mapping_to_disk()
            .unwrap_or_else(|error| panic!("mapping save failed: {error}"));
    } else {
        heddle_git_projection::git_ingest::import_git_history(
            bridge,
            Some(source),
            &[],
            ingest::ImportOptions::default(),
            None,
        )
        .unwrap_or_else(|error| panic!("bridge import failed: {error}"));
    }
}

/// Repack the native store, so the layout trees must survive the packed
/// cutover (they ride the native lane, not the columnar NPK1 pack).
fn repack(repo: &Repository) {
    let scheduler = RepackScheduler::new(
        RepackPolicy::default(),
        RepackResourceLimits::new(NonZeroUsize::MIN).with_io_rate(None),
    );
    let operation = Arc::new(FsRepackOperation::new(repo.store().clone()));
    let RepackSchedule::Started(handle) = scheduler.repack_now(operation).expect("repack") else {
        panic!("native repack did not start");
    };
    handle.wait().expect("native repack");
}

fn assert_exact_roundtrip(direct: bool, repacked: bool) {
    let source_home = TempDir::new().expect("source");
    let source = source_home.path();
    let branches = build_corpus(source);
    let source_objects = every_commit_and_tree(source);

    let heddle_home = TempDir::new().expect("heddle");
    let repo = Repository::init(heddle_home.path()).expect("init heddle repo");
    let mut bridge = GitProjection::new(&repo);
    import(&mut bridge, &repo, source, direct);
    if repacked {
        repack(&repo);
    }

    let dest_home = TempDir::new().expect("dest");
    let dest = dest_home.path().join("export");
    let stats = bridge
        .export_to_path(&dest)
        .unwrap_or_else(|error| panic!("export failed: {error}"));
    assert!(
        stats.failed_refs.is_empty(),
        "failed exports: {:?}",
        stats.failed_refs
    );

    for (branch, tip) in &branches {
        let exported = git(&dest, &["rev-parse", &format!("refs/heads/{branch}")]);
        assert_eq!(
            &exported, tip,
            "branch {branch} exported to a different commit (direct={direct}, repacked={repacked})"
        );
    }
    let exported_objects = every_commit_and_tree(&dest);
    for oid in &source_objects {
        assert!(
            exported_objects.contains(oid),
            "object {oid} missing from export (direct={direct}, repacked={repacked})"
        );
    }
}

#[test]
fn noncanonical_trees_roundtrip_exactly_through_direct_ingest() {
    assert_exact_roundtrip(true, false);
}

#[test]
fn noncanonical_trees_roundtrip_exactly_through_projection_import() {
    assert_exact_roundtrip(false, false);
}

#[test]
fn noncanonical_trees_roundtrip_exactly_after_native_repack() {
    assert_exact_roundtrip(true, true);
}

#[test]
fn duplicate_tree_entry_names_are_rejected_naming_the_tree() {
    let source_home = TempDir::new().expect("source");
    let source = source_home.path();
    git(source, &["init", "-q", "--initial-branch=main"]);
    let a = blob(source, "a\n");
    let b = blob(source, "b\n");
    let tree = raw_tree(source, &[("100644", "same", &a), ("100644", "same", &b)]);
    let tip = commit(source, &tree, None, "duplicate names");
    git(source, &["update-ref", "refs/heads/main", &tip]);

    let heddle_home = TempDir::new().expect("heddle");
    let error = ingest::import_git_into_scoped_with_options(
        source,
        heddle_home.path(),
        ingest::ImportOptions::default(),
        ingest::ImportScope::all(),
    )
    .expect_err("a tree with duplicate names must not import");
    let message = error.to_string();
    assert!(
        message.contains(&tree),
        "error must name the tree {tree}: {message}"
    );
    assert!(
        message.contains("same"),
        "error must name the duplicate entry: {message}"
    );
}
