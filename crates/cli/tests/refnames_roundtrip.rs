// SPDX-License-Identifier: Apache-2.0
//! Git import identities survive the native storage and local fetch path.
use std::{path::Path, process::Command};

use cli::{ObjectStore, Repository};
use hosted_client::client::LocalSync;
use objects::{name_encoding::git_name, object::ThreadName};
use refs::Head;
use tempfile::TempDir;

#[test]
fn long_solid_checkout_keeps_exact_identity_and_drops_cleanly() {
    let root = TempDir::new().expect("native repository");
    let home = TempDir::new().expect("CLI home");
    let repo = Repository::init_default(root.path()).expect("repository");
    let mut config = repo.config().clone();
    config.set_principal("Refname Test", "refnames@heddle.test");
    config
        .save(&repo.heddle_dir().join("config.toml"))
        .expect("identity");
    let name = format!("{}é", "界".repeat(337));
    for args in [
        vec!["start", &name, "--workspace", "solid"],
        vec!["thread", "drop", &name],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_heddle"))
            .args(["--output", "json", "-C"])
            .arg(root.path())
            .args(&args)
            .env("HEDDLE_HOME", home.path())
            .output()
            .expect("CLI lifecycle");
        assert!(
            output.status.success(),
            "{args:?}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if args[0] == "start" {
            let checkout = repo.managed_checkout_path(&name);
            assert!(checkout.join(".heddle/objectstore").is_file());
            assert!(checkout.as_os_str().len() <= 1024);
            let reopened = Repository::open(&checkout).expect("long checkout opens");
            assert_eq!(
                reopened.head_ref().expect("exact HEAD"),
                Head::Attached {
                    thread: ThreadName::new(&name)
                }
            );
            objects::name_encoding::verify_name_entry(&repo.heddle_dir().join("threads"), &name)
                .expect("checkout digest identity");
        }
    }
    assert!(!repo.managed_checkout_path(&name).exists());
}

#[test]
fn native_git_export_and_reimport_are_collision_free() {
    let source = TempDir::new().expect("native source");
    let exported = TempDir::new().expect("Git export");
    let imported = TempDir::new().expect("native reimport");
    let exported_path = exported.path().join("export.git");
    let repo = Repository::init_default(source.path()).expect("repository");
    let mut config = repo.config().clone();
    config.set_principal("Refname Test", "refnames@heddle.test");
    config
        .save(&repo.heddle_dir().join("config.toml"))
        .expect("identity");
    let repo = Repository::open(source.path()).expect("configured repository");
    let first = repo.head().expect("seed").expect("state");
    assert!(
        repo.refs()
            .set_thread(&ThreadName::new("git%foo"), &first)
            .is_err()
    );
    let native = ThreadName::new("foo");
    repo.set_thread_recorded(&native, &first)
        .expect("native thread");
    std::fs::write(source.path().join("payload.txt"), "imported tip\n").expect("payload");
    let second = repo
        .snapshot(Some("second".into()), None)
        .expect("snapshot")
        .state_id;
    let mapped = ThreadName::from_git_branch("git%foo").expect("import identity");
    repo.set_thread_recorded(&mapped, &second)
        .expect("imported thread");
    let mut projection = heddle_git_projection::GitProjection::new(&repo);
    projection.export_to_path(&exported_path).expect("export");
    assert_ne!(
        git(&exported_path, &["rev-parse", "refs/heads/foo"]),
        git(&exported_path, &["rev-parse", "refs/heads/git%foo"])
    );
    let native_oid = git(&exported_path, &["rev-parse", "refs/heads/foo"]);
    let imported_oid = git(&exported_path, &["rev-parse", "refs/heads/git%foo"]);
    git(&exported_path, &["fsck", "--full", "--strict"]);
    let (_, map) = ingest::import_git_into(&exported_path, imported.path()).expect("reimport");
    let reimported = Repository::open(imported.path()).expect("native reimport");
    assert_eq!(
        reimported.refs().list_threads().expect("threads"),
        repo.refs().list_threads().expect("original threads")
    );
    assert_eq!(
        reimported.refs().get_thread(&native).expect("native tip"),
        map.get_commit(native_oid.trim_end_matches('\n'))
            .expect("native source tip mapping")
    );
    assert_eq!(
        reimported.refs().get_thread(&mapped).expect("imported tip"),
        map.get_commit(imported_oid.trim_end_matches('\n'))
            .expect("imported source tip mapping")
    );
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "Refname Test")
        .env("GIT_AUTHOR_EMAIL", "refnames@heddle.test")
        .env("GIT_COMMITTER_NAME", "Refname Test")
        .env("GIT_COMMITTER_EMAIL", "refnames@heddle.test")
        .output()
        .expect("run Git");
    assert!(
        output.status.success(),
        "Git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("Git UTF-8 output")
}

fn round_trip(name: &str) {
    round_trip_names(&[name]);
}

fn round_trip_names(names: &[&str]) {
    let source = TempDir::new().expect("Git source");
    let native = TempDir::new().expect("native source");
    let destination = TempDir::new().expect("fetch destination");
    git(source.path(), &["init", "-b", "main"]);
    std::fs::write(source.path().join("payload.txt"), "exact branch identity\n").expect("payload");
    git(source.path(), &["add", "."]);
    git(source.path(), &["commit", "-m", "fixture"]);
    let oid = git(source.path(), &["rev-parse", "HEAD"]);
    let oid = oid.trim_end_matches('\n');
    for name in names {
        let full = format!("refs/heads/{name}");
        if name.len() > 240 {
            std::fs::write(
                source.path().join(".git/packed-refs"),
                format!("{oid} {full}\n"),
            )
            .expect("packed long Git ref");
        } else {
            git(source.path(), &["update-ref", &full, oid]);
        }
    }
    ingest::import_git_into(source.path(), native.path()).expect("all-branch import");
    let target = Repository::init(destination.path()).expect("fresh receiver");
    for name in names {
        let source_repo = Repository::open(native.path()).expect("native repository");
        let thread = ThreadName::from_git_branch(name).expect("Git boundary mapping");
        assert_eq!(git_name(&thread), *name);
        let state = source_repo
            .refs()
            .get_thread(&thread)
            .expect("stored ref")
            .expect("imported tip");
        let head = Head::Attached {
            thread: thread.clone(),
        };
        source_repo
            .refs()
            .write_head(&head)
            .expect("attach native HEAD");
        assert_eq!(source_repo.refs().read_head().expect("exact HEAD"), head);
        source_repo.refs().pack_refs().expect("native packed refs");
        drop(source_repo);
        let sync = LocalSync::open(native.path()).expect("reopen for fetch");
        assert!(
            sync.list_threads()
                .expect("advertised listing")
                .contains(&(thread.to_string(), state))
        );
        let home = TempDir::new().expect("receiver home");
        let output = Command::new(env!("CARGO_BIN_EXE_heddle"))
            .args(["--output", "json", "-C"])
            .arg(destination.path())
            .arg("pull")
            .arg(native.path())
            .args(["--thread", thread.as_str()])
            .env("HEDDLE_HOME", home.path())
            .output()
            .expect("real receiver pull");
        assert!(
            output.status.success(),
            "receiver pull: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            target.refs().get_thread(&thread).expect("receiver ref"),
            Some(state)
        );
        assert!(
            target
                .store()
                .get_state(&state)
                .expect("fetched state")
                .is_some()
        );
        let tree = target
            .store()
            .get_state(&state)
            .expect("state read")
            .expect("state")
            .tree;
        assert!(
            target
                .store()
                .get_tree(&tree)
                .expect("fetched tree")
                .is_some()
        );
    }
}

#[test]
fn import_storage_head_pack_list_fetch_equals() {
    round_trip("feat/mcp=timeout");
}
#[test]
fn import_storage_head_pack_list_fetch_comma() {
    round_trip("a,b");
}
#[test]
fn import_storage_head_pack_list_fetch_unicode() {
    round_trip("ünicode/ブランチ");
}
#[test]
fn import_storage_head_pack_list_fetch_at() {
    round_trip("@");
}
#[test]
fn import_storage_head_pack_list_fetch_plus() {
    round_trip("x+y");
}
#[test]
fn import_storage_head_pack_list_fetch_nbsp() {
    round_trip("trailing\u{a0}");
}
#[test]
fn import_storage_head_pack_list_fetch_replacement() {
    round_trip("literal\u{fffd}");
}
#[test]
fn import_storage_head_pack_list_fetch_long() {
    round_trip(&"界".repeat(337));
}
#[test]
fn import_storage_head_pack_list_fetch_reserved() {
    round_trip("heddle/foo");
    round_trip(&format!("heddle/{}", "界".repeat(333)));
}

#[test]
fn import_storage_head_pack_list_fetch_case_pair() {
    round_trip_names(&["Foo", "foo"]);
}
#[test]
fn import_storage_head_pack_list_fetch_normalization_pair() {
    round_trip_names(&["caf\u{e9}", "cafe\u{301}"]);
}
#[test]
fn import_storage_head_pack_list_fetch_literal_git_prefix() {
    round_trip("git%foo");
}

#[cfg(unix)]
#[test]
fn projection_import_reports_non_utf8_branch_and_ignores_other_namespaces() {
    fn invalid_source() -> TempDir {
        let source = TempDir::new().expect("Git source");
        git(source.path(), &["init", "-b", "main"]);
        std::fs::write(source.path().join("payload"), "payload").expect("payload");
        git(source.path(), &["add", "."]);
        git(source.path(), &["commit", "-m", "fixture"]);
        let oid = git(source.path(), &["rev-parse", "HEAD"]);
        let oid = oid.trim_end_matches('\n');
        let mut packed = Vec::new();
        for name in [
            b"refs/heads/main".as_slice(),
            "refs/heads/bad-\u{fffd}".as_bytes(),
            b"refs/heads/bad-\xff".as_slice(),
            b"refs/remotes/origin/bad-\xff",
        ] {
            packed.extend_from_slice(oid.as_bytes());
            packed.extend_from_slice(b" ");
            packed.extend_from_slice(name);
            packed.push(b'\n');
        }
        std::fs::write(source.path().join(".git/packed-refs"), packed).expect("packed raw names");
        std::fs::remove_file(source.path().join(".git/refs/heads/main")).expect("packed main only");
        source
    }
    let source = invalid_source();
    let native = TempDir::new().expect("native destination");
    let repo = Repository::init(native.path()).expect("native repo");
    let mut projection = heddle_git_projection::GitProjection::new(&repo);
    let stats = heddle_git_projection::git_ingest::import_git_history(
        &mut projection,
        Some(source.path()),
        &[],
        ingest::ImportOptions::default(),
        None,
    )
    .expect("complete projection import");
    let excluded = stats
        .skipped_refs
        .iter()
        .find(|excluded| excluded.raw_name == b"refs/heads/bad-\xff")
        .expect("excluded branch named in report");
    assert!(excluded.reason.description().contains("not valid UTF-8"));
    assert_eq!(excluded.display_name(), "refs/heads/bad-\\xff");
    assert!(
        repo.refs()
            .get_thread(&ThreadName::new("bad-\u{fffd}"))
            .expect("valid replacement character")
            .is_some()
    );
    assert!(
        repo.refs()
            .get_thread(&ThreadName::new("main"))
            .expect("main")
            .is_some()
    );
    let home = TempDir::new().expect("CLI import home");
    for json in [true, false] {
        let cli_source = invalid_source();
        let mut command = Command::new(env!("CARGO_BIN_EXE_heddle"));
        if json {
            command.args(["--output", "json"]);
        }
        let output = command
            .arg("-C")
            .arg(cli_source.path())
            .args(["import", "local"])
            .env("HEDDLE_HOME", home.path())
            .output()
            .expect("public import command");
        assert!(
            output.status.success(),
            "public import: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if json {
            let report: serde_json::Value =
                serde_json::from_slice(&output.stdout).expect("JSON import report");
            let skipped = report["skipped_refs"].as_array().expect("exclusions");
            assert!(skipped.iter().any(|entry| {
                entry["name"]
                    .as_str()
                    .is_some_and(|name| name == "refs/heads/bad-\\xff")
                    && entry["reason"]
                        .as_str()
                        .is_some_and(|reason| reason.contains("not valid UTF-8"))
            }));
            assert!(!skipped.iter().any(|entry| {
                entry["name"]
                    .as_str()
                    .is_some_and(|name| name.starts_with("refs/remotes/"))
            }));
        } else {
            let report = String::from_utf8(output.stdout).expect("human report");
            assert!(report.contains("refs/heads/bad-\\xff") && report.contains("not valid UTF-8"));
        }
    }
}
