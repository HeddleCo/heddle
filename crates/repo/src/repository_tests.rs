// SPDX-License-Identifier: Apache-2.0
use std::{
    fs,
    path::{Path, PathBuf},
};

use objects::{
    object::{Blob, ThreadName, Tree, TreeEntry},
    store::{ObjectStore, ShallowInfo},
    util::{gitlink_placeholder_bytes, symlink_target_bytes},
};
use oplog::{OpLog, OpLogBackend, OpRecord};
use refs::{Head, RefManager};
use serde_json::json;
use sley::{ObjectFormat as GitObjectFormat, ObjectId as GitObjectId};
use tempfile::TempDir;

use super::{
    repo_config::SUPPORTED_REPO_FORMAT,
    repository_snapshot::{SnapshotFault, with_snapshot_fault},
};
use crate::{
    ChangedPathFilters, HeddleError, HistoryQuery, RepoConfig, Repository, RepositoryCapability,
    RepositorySourceAuthority, ThreadFreshness, ThreadManager, WorktreeIndex,
};

fn create_test_repo() -> (TempDir, Repository) {
    let temp_dir = TempDir::new().unwrap();
    let repo = Repository::init_default(temp_dir.path()).unwrap();
    (temp_dir, repo)
}

#[derive(Debug, PartialEq, Eq)]
enum TreeEntrySnapshot {
    Directory,
    File(Vec<u8>),
    Symlink(PathBuf),
}

fn snapshot_directory(root: &Path) -> Vec<(PathBuf, TreeEntrySnapshot)> {
    fn visit(root: &Path, directory: &Path, snapshot: &mut Vec<(PathBuf, TreeEntrySnapshot)>) {
        let mut entries = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        entries.sort();

        for path in entries {
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            let file_type = fs::symlink_metadata(&path).unwrap().file_type();
            if file_type.is_dir() {
                snapshot.push((relative, TreeEntrySnapshot::Directory));
                visit(root, &path, snapshot);
            } else if file_type.is_file() {
                snapshot.push((relative, TreeEntrySnapshot::File(fs::read(path).unwrap())));
            } else if file_type.is_symlink() {
                snapshot.push((
                    relative,
                    TreeEntrySnapshot::Symlink(fs::read_link(path).unwrap()),
                ));
            } else {
                panic!(
                    "unsupported filesystem entry in repository snapshot: {}",
                    path.display()
                );
            }
        }
    }

    let mut snapshot = Vec::new();
    visit(root, root, &mut snapshot);
    snapshot
}

fn decode_hex_fixture(source: &str) -> Vec<u8> {
    let compact = source
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect::<Vec<_>>();
    assert_eq!(compact.len() % 2, 0, "hex fixture must contain byte pairs");
    compact
        .chunks_exact(2)
        .map(|pair| {
            let digits = std::str::from_utf8(pair).unwrap();
            u8::from_str_radix(digits, 16).unwrap()
        })
        .collect()
}

#[cfg(unix)]
#[test]
fn init_creates_heddle_dir_with_mode_0700() {
    use std::os::unix::fs::PermissionsExt;

    let temp_dir = TempDir::new().unwrap();
    let repo = Repository::init(temp_dir.path()).unwrap();
    let mode = fs::metadata(repo.heddle_dir())
        .expect("heddle dir metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700, ".heddle must be owner-only, got {mode:o}");
}

fn open_test_repo_with_store<S: ObjectStore>(
    heddle_dir: impl AsRef<Path>,
    store: S,
) -> Repository<RefManager, OpLog, S> {
    let heddle_dir = heddle_dir.as_ref().to_path_buf();
    let root = heddle_dir
        .parent()
        .expect("test heddle dir should live under a worktree root")
        .to_path_buf();
    let config = RepoConfig::load(&heddle_dir.join("config.toml")).unwrap();
    let refs = RefManager::new(&heddle_dir);
    let oplog = OpLog::new_unattributed(&heddle_dir);
    let shallow = ShallowInfo::load(&heddle_dir).unwrap();
    Repository::from_parts(root, heddle_dir, store, refs, oplog, config, shallow)
}

fn gitlink_target_for_tests() -> GitObjectId {
    GitObjectId::from_hex(
        GitObjectFormat::Sha1,
        "1234567890abcdef1234567890abcdef12345678",
    )
    .unwrap()
}

#[cfg(unix)]
fn handcrafted_symlink_tree(repo: &Repository, target: &std::path::Path) -> Tree {
    let blob = Blob::new(symlink_target_bytes(target));
    let hash = repo.store().put_blob(&blob).unwrap();
    Tree::from_entries(vec![TreeEntry::symlink("link", hash).unwrap()])
}

#[test]
fn test_init_creates_structure() {
    let temp_dir = TempDir::new().unwrap();
    let repo = Repository::init_default(temp_dir.path()).unwrap();

    assert!(temp_dir.path().join(".heddle").exists());
    assert!(temp_dir.path().join(".heddle/config.toml").exists());
    assert!(temp_dir.path().join(".heddle/objects/blobs").exists());
    assert!(temp_dir.path().join(".heddle/objects/trees").exists());
    assert!(temp_dir.path().join(".heddle/objects/states").exists());
    let root_state = repo.head().unwrap().expect("init should seed main state");
    assert_eq!(
        repo.refs().get_thread(&ThreadName::new("main")).unwrap(),
        Some(root_state)
    );
    let state = repo.store().get_state(&root_state).unwrap().unwrap();
    assert!(state.parents.is_empty());
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();
    assert!(tree.is_empty());
}

#[test]
fn test_custom_store_fixture_threads_a_custom_object_store() {
    // heddle#283: keep coverage that `Repository<RefManager, OpLog, S>`
    // compiles and runs with a concrete store that is not the default
    // `AnyStore`, without exposing a production custom-store open helper.
    use objects::store::FsStore;

    let temp_dir = TempDir::new().unwrap();
    let repo = Repository::init_default(temp_dir.path()).unwrap();
    let heddle_dir = repo.heddle_dir().to_path_buf();
    drop(repo);

    let store = FsStore::new(&heddle_dir);
    let repo: Repository<_, _, FsStore> = open_test_repo_with_store(&heddle_dir, store);

    let blob = objects::object::Blob::from("custom store round-trip");
    let hash = repo.store().put_blob(&blob).unwrap();
    assert_eq!(
        repo.store().get_blob(&hash).unwrap().unwrap().content(),
        blob.content()
    );
}

#[test]
fn open_refuses_newer_repository_format_with_recovery_advice() {
    let temp_dir = TempDir::new().unwrap();
    Repository::init_default(temp_dir.path()).unwrap();

    let config_path = temp_dir.path().join(".heddle/config.toml");
    fs::write(&config_path, "[repository]\nversion = 99\n").unwrap();

    let err = match Repository::open(temp_dir.path()) {
        Ok(_) => panic!("newer repo format must refuse"),
        Err(err) => err,
    };
    match &err {
        HeddleError::RepositoryFormatTooNew {
            path,
            found,
            supported,
        } => {
            assert_eq!(
                path.canonicalize().unwrap(),
                config_path.canonicalize().unwrap()
            );
            assert_eq!(*found, 99);
            assert_eq!(*supported, SUPPORTED_REPO_FORMAT);
        }
        other => panic!("expected RepositoryFormatTooNew, got {other:?}"),
    }

    let message = err.to_string();
    assert!(
        message.contains("repository format 99"),
        "error should name found format: {message}"
    );
    assert!(
        message.contains(&format!("this binary supports {SUPPORTED_REPO_FORMAT}")),
        "error should name supported format: {message}"
    );
    assert!(
        message.contains("upgrade heddle or run `heddle migrate`"),
        "error should include recovery advice: {message}"
    );
}

#[test]
fn open_accepts_supported_repository_format() {
    let temp_dir = TempDir::new().unwrap();
    Repository::init_default(temp_dir.path()).unwrap();

    let config_path = temp_dir.path().join(".heddle/config.toml");
    fs::write(
        &config_path,
        format!("[repository]\nversion = {SUPPORTED_REPO_FORMAT}\nsource_authority = \"native\"\n"),
    )
    .unwrap();

    Repository::open(temp_dir.path()).expect("supported repo format should open");
}

#[test]
fn open_refuses_v2_as_migration_required_without_rewriting_fixture() {
    let temp_dir = TempDir::new().unwrap();
    Repository::init_default(temp_dir.path()).unwrap();

    let config_path = temp_dir.path().join(".heddle/config.toml");
    let fixture = include_str!("../tests/fixtures/repository-v2/config.toml");
    fs::write(&config_path, fixture).unwrap();

    let error = match Repository::open(temp_dir.path()) {
        Ok(_) => panic!("v2 repository format must require migration"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        HeddleError::RepositoryFormatMigrationRequired {
            found: 2,
            required: SUPPORTED_REPO_FORMAT,
            ..
        }
    ));
    assert_eq!(fs::read_to_string(config_path).unwrap(), fixture);
}

#[test]
fn open_refuses_legacy_oplog_before_mutating_repository() {
    let temp_dir = TempDir::new().unwrap();
    let repo = Repository::init_default(temp_dir.path()).unwrap();
    assert_eq!(repo.config().repository.version, SUPPORTED_REPO_FORMAT);

    let heddle_dir = repo.heddle_dir().to_path_buf();
    let oplog_path = heddle_dir.join("oplog/oplog.bin");
    drop(repo);

    let legacy_oplog = decode_hex_fixture(include_str!(
        "../tests/fixtures/issue-449-legacy-pre-atomic/oplog.bin.hex"
    ));
    assert_eq!(legacy_oplog.len(), 704, "historical fixture length changed");
    assert_eq!(&legacy_oplog[..12], b"LMOPLOG\0\x02\0\0\0");
    fs::write(&oplog_path, legacy_oplog).unwrap();
    let before = snapshot_directory(&heddle_dir);

    let error = match Repository::open(temp_dir.path()) {
        Ok(_) => panic!("legacy oplog must block repository open"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        HeddleError::StorageFormatMigrationRequired {
            ref storage,
            found: 2,
            required: 4,
        } if storage == "packed oplog container"
    ));
    assert_eq!(
        snapshot_directory(&heddle_dir),
        before,
        "refusing a legacy oplog must not mutate any .heddle entry"
    );
}

/// Mutating commands historically bootstrap plain Git via `Repository::open`.
/// Observe-only CLI paths (status/verify/doctor) must not call open until a
/// `.heddle` sidecar already exists — see `verify_execution_context_from_cli`.
#[test]
fn open_bootstraps_plain_git_sidecar_for_mutators() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path();
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

    let repo = Repository::open(root).expect("open should bootstrap plain Git for mutators");
    assert!(
        root.join(".heddle").is_dir(),
        "open should create the Heddle sidecar for mutators"
    );
    assert_eq!(
        repo.capability(),
        crate::RepositoryCapability::GitOverlay,
        "bootstrapped plain Git should be git-overlay"
    );
}

#[test]
fn explicit_native_authority_survives_alongside_git_metadata() {
    let temp_dir = TempDir::new().unwrap();
    sley::Repository::init(temp_dir.path()).unwrap();

    let repo = Repository::init_default(temp_dir.path()).unwrap();
    assert_eq!(repo.capability(), RepositoryCapability::NativeHeddle);
    drop(repo);

    let reopened = Repository::open(temp_dir.path()).unwrap();
    assert_eq!(reopened.capability(), RepositoryCapability::NativeHeddle);
    assert!(temp_dir.path().join(".git").is_dir());
}

#[test]
fn open_refuses_v1_before_reading_legacy_objects() {
    let temp_dir = TempDir::new().unwrap();
    let repo = Repository::init_default(temp_dir.path()).unwrap();
    let ledger = repo.heddle_dir().join("state/schema_versions.toml");
    if ledger.exists() {
        fs::remove_file(&ledger).unwrap();
    }

    let config_path = repo.heddle_dir().join("config.toml");
    fs::write(&config_path, "[repository]\nversion = 1\n").unwrap();

    drop(repo);

    let err = match Repository::open(temp_dir.path()) {
        Ok(_) => panic!("legacy repository format must block open"),
        Err(err) => err,
    };
    assert!(matches!(
        err,
        HeddleError::RepositoryFormatMigrationRequired {
            found: 1,
            required: SUPPORTED_REPO_FORMAT,
            ..
        }
    ));
}

#[test]
fn test_init_fails_if_exists() {
    let temp_dir = TempDir::new().unwrap();
    Repository::init_default(temp_dir.path()).unwrap();

    let result = Repository::init_default(temp_dir.path());
    assert!(result.is_err());
}

#[test]
fn test_set_shallow_updates_memory_and_persists() {
    let (temp_dir, repo) = create_test_repo();
    let state_id = crate::test_state_id();

    repo.set_shallow(&state_id, &[]).unwrap();

    assert!(repo.is_shallow(&state_id));

    let reopened = Repository::open(temp_dir.path()).unwrap();
    assert!(reopened.is_shallow(&state_id));
}

#[test]
fn test_open_finds_repo() {
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path().canonicalize().unwrap();
    Repository::init_default(&temp_path).unwrap();

    let sub = temp_path.join("foo/bar");
    fs::create_dir_all(&sub).unwrap();

    let repo = Repository::open(&sub).unwrap();
    assert_eq!(repo.root(), temp_path);
}

#[test]
fn test_snapshot_creates_state() {
    let (temp_dir, repo) = create_test_repo();
    let initial_head = repo.head().unwrap().expect("init should seed main state");

    fs::write(temp_dir.path().join("hello.txt"), "world").unwrap();

    let state = repo
        .snapshot(Some("Initial commit".to_string()), None)
        .unwrap();

    assert_eq!(state.intent, Some("Initial commit".to_string()));
    assert_eq!(state.parents, vec![initial_head]);

    let head = repo.head().unwrap();
    assert_eq!(head, Some(state.id()));
}

#[test]
fn courtesy_filename_scoped_root_only() {
    // heddle#316 #9: the courtesy-stub filename is reserved ROOT-ANCHORED
    // (`/HEDDLE-EMBARGO.txt`). The stub is only ever written at the worktree
    // root, so a root-level file of that name (an operator-local under-tier
    // stub) stays ignored, but a user's OWN `sub/HEDDLE-EMBARGO.txt` deeper in
    // the tree must be CAPTURED — the bare filename would have gitignore-matched
    // it at any depth and silently dropped it.
    let (temp_dir, repo) = create_test_repo();
    let root = temp_dir.path();

    fs::write(root.join("HEDDLE-EMBARGO.txt"), "root stub").unwrap();
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("sub").join("HEDDLE-EMBARGO.txt"), "user content").unwrap();

    let state = repo.snapshot(Some("capture".to_string()), None).unwrap();
    let tree = repo
        .store()
        .get_tree(&state.tree)
        .unwrap()
        .expect("snapshot tree");

    assert!(
        !tree
            .entries()
            .iter()
            .any(|e| e.name() == "HEDDLE-EMBARGO.txt"),
        "the root-level courtesy stub must stay ignored at the worktree root"
    );
    let sub = tree
        .entries()
        .iter()
        .find(|e| e.name() == "sub" && e.is_tree())
        .expect("sub/ subtree must be captured");
    let sub_hash = sub.tree_hash().expect("sub is a tree");
    let sub_tree = repo
        .store()
        .get_tree(&sub_hash)
        .unwrap()
        .expect("sub subtree");
    assert!(
        sub_tree
            .entries()
            .iter()
            .any(|e| e.name() == "HEDDLE-EMBARGO.txt"),
        "a user's own sub/HEDDLE-EMBARGO.txt must be captured (root-anchored ignore)"
    );
}

#[test]
fn snapshot_packs_blobs_and_leaves_no_loose_blob_files() {
    // ACID + perf invariant: a successful snapshot must install
    // every new blob through the pack hot path, not as N loose
    // files. If something regresses to per-blob loose writes the
    // snapshot quietly goes back to fsync-per-file and the
    // performance test will catch it later — but this test catches
    // it now, in the unit suite, with a clear assertion.
    let (temp_dir, repo) = create_test_repo();

    for i in 0..50 {
        fs::write(
            temp_dir.path().join(format!("file_{i}.txt")),
            format!("content {i}\n"),
        )
        .unwrap();
    }
    let state = repo.snapshot(None, None).unwrap();

    let blobs_dir = temp_dir.path().join(".heddle/objects/blobs");
    let loose_count = fs::read_dir(&blobs_dir)
        .map(|iter| iter.count())
        .unwrap_or(0);
    assert_eq!(
        loose_count, 0,
        "snapshot left {loose_count} loose blob shards behind; expected 0 (everything in a pack)",
    );

    // The state itself must be reachable through the ref the
    // snapshot returned — no orphaned commit.
    let head = repo.head().unwrap();
    assert_eq!(head, Some(state.id()));
}

#[test]
fn supplied_tree_snapshot_batches_new_blobs_into_a_pack() {
    let (temp_dir, repo) = create_test_repo();
    let blob = Blob::from_slice(b"structured native authoring");
    let hash = blob.hash();
    let tree = Tree::from_entries(vec![TreeEntry::file("agent.txt", hash, false).unwrap()]);

    let execution = repo
        .snapshot_tree_with_blobs_with_attribution_profiled(
            tree,
            vec![blob],
            Some("structured snapshot".to_string()),
            None,
            repo.get_attribution().unwrap(),
        )
        .unwrap();

    assert_eq!(repo.head().unwrap(), Some(execution.state.id()));
    assert_eq!(
        repo.store().get_blob(&hash).unwrap().unwrap().content(),
        b"structured native authoring"
    );
    let blobs_dir = temp_dir.path().join(".heddle/objects/blobs");
    assert_eq!(
        fs::read_dir(blobs_dir)
            .map(|entries| entries.count())
            .unwrap_or_default(),
        0,
        "structured snapshot should not fan new blobs out into loose files"
    );
}

#[test]
fn snapshot_preserves_unchanged_materialized_gitlink_placeholder() {
    let (temp_dir, repo) = create_test_repo();
    let target = gitlink_target_for_tests();
    let tree = Tree::from_entries(vec![TreeEntry::gitlink("vendor", target).unwrap()]);
    let baseline = repo
        .snapshot_tree_with_attribution_profiled(
            tree.clone(),
            Some("gitlink baseline".to_string()),
            None,
            repo.get_attribution().unwrap(),
        )
        .unwrap()
        .state;

    repo.materialize_computed_tree(&tree, temp_dir.path())
        .unwrap();
    assert_eq!(
        fs::read(temp_dir.path().join("vendor")).unwrap(),
        gitlink_placeholder_bytes(&target)
    );

    let recaptured = repo
        .snapshot(Some("recapture unchanged gitlink".to_string()), None)
        .unwrap();
    assert_eq!(recaptured.parents, vec![baseline.id()]);
    let recaptured_tree = repo
        .store()
        .get_tree(&recaptured.tree)
        .unwrap()
        .expect("recaptured tree");
    let entry = recaptured_tree.get("vendor").expect("vendor entry");
    assert_eq!(entry.gitlink_target(), Some(target));
}

#[test]
fn snapshot_captures_edited_gitlink_placeholder_as_blob() {
    let (temp_dir, repo) = create_test_repo();
    let target = gitlink_target_for_tests();
    let tree = Tree::from_entries(vec![TreeEntry::gitlink("vendor", target).unwrap()]);
    repo.snapshot_tree_with_attribution_profiled(
        tree.clone(),
        Some("gitlink baseline".to_string()),
        None,
        repo.get_attribution().unwrap(),
    )
    .unwrap();
    repo.materialize_computed_tree(&tree, temp_dir.path())
        .unwrap();

    fs::write(temp_dir.path().join("vendor"), "this is a real file now").unwrap();

    let recaptured = repo
        .snapshot(
            Some("recapture edited gitlink placeholder".to_string()),
            None,
        )
        .unwrap();
    let recaptured_tree = repo
        .store()
        .get_tree(&recaptured.tree)
        .unwrap()
        .expect("recaptured tree");
    let entry = recaptured_tree.get("vendor").expect("vendor entry");
    let blob_hash = entry.blob_hash().expect("edited placeholder becomes blob");
    let blob = repo
        .store()
        .get_blob(&blob_hash)
        .unwrap()
        .expect("captured edited placeholder blob");
    assert_eq!(blob.content(), b"this is a real file now");
}

#[test]
fn snapshot_captures_placeholder_bytes_without_gitlink_baseline_as_blob() {
    let (temp_dir, repo) = create_test_repo();
    let target = gitlink_target_for_tests();
    fs::write(
        temp_dir.path().join("vendor"),
        gitlink_placeholder_bytes(&target),
    )
    .unwrap();

    let state = repo
        .snapshot(Some("capture placeholder-looking file".to_string()), None)
        .unwrap();
    let tree = repo
        .store()
        .get_tree(&state.tree)
        .unwrap()
        .expect("snapshot tree");
    let entry = tree.get("vendor").expect("vendor entry");
    let blob_hash = entry.blob_hash().expect("placeholder bytes remain blob");
    let blob = repo
        .store()
        .get_blob(&blob_hash)
        .unwrap()
        .expect("captured placeholder-looking blob");
    assert_eq!(blob.content(), gitlink_placeholder_bytes(&target));
}

#[test]
fn materialize_tree_keeps_legacy_gitlink_marker_blob_as_file() {
    let (temp_dir, repo) = create_test_repo();
    let marker = b"heddle-submodule: 0303030303030303030303030303030303030303\n";
    let blob_hash = repo
        .store()
        .put_blob(&Blob::new(marker.to_vec()))
        .expect("blob");
    let tree = Tree::from_entries(vec![
        TreeEntry::file("vendor", blob_hash, false).expect("blob entry"),
    ]);

    repo.materialize_computed_tree(&tree, temp_dir.path())
        .expect("materialize marker blob");

    assert_eq!(
        fs::read(temp_dir.path().join("vendor")).expect("materialized blob"),
        marker,
        "legacy marker bytes are ordinary file content at runtime"
    );
}

#[test]
fn compare_worktree_cached_treats_materialized_gitlink_as_clean_leaf() {
    let (temp_dir, repo) = create_test_repo();
    let target = gitlink_target_for_tests();
    let tree = Tree::from_entries(vec![TreeEntry::gitlink("vendor", target).unwrap()]);

    repo.materialize_computed_tree(&tree, temp_dir.path())
        .expect("materialize gitlink placeholder");

    let status = repo.compare_worktree_cached(&tree).expect("status");
    assert!(status.modified.is_empty(), "modified={:?}", status.modified);
    assert!(status.added.is_empty(), "added={:?}", status.added);
    assert!(status.deleted.is_empty(), "deleted={:?}", status.deleted);
    assert_eq!(
        fs::read(temp_dir.path().join("vendor")).expect("gitlink placeholder"),
        gitlink_placeholder_bytes(&target)
    );
}

#[test]
fn snapshot_failure_leaves_ref_unchanged() {
    // ACID atomicity: a failed snapshot must not advance the head.
    // Stage an unresolved merge — `snapshot()` checks for one up
    // front and returns `HeddleError::Conflict` before any writes —
    // then assert the head is identical to its pre-call value.
    use objects::object::StateId;

    let (temp_dir, repo) = create_test_repo();
    fs::write(temp_dir.path().join("a.txt"), "a").unwrap();
    let baseline = repo.snapshot(None, None).unwrap();

    let theirs = StateId::from_bytes([0xff; 32]);
    repo.merge_state_manager()
        .start(
            baseline.id(),
            theirs,
            None,
            vec!["unresolved.txt".to_string()],
        )
        .unwrap();

    fs::write(temp_dir.path().join("b.txt"), "b").unwrap();
    let result = repo.snapshot(Some("would-fail".to_string()), None);
    assert!(matches!(result, Err(HeddleError::Conflict(_))));

    // Head must still point at the baseline state — not at any
    // half-written successor.
    let head_after = repo.head().unwrap();
    assert_eq!(head_after, Some(baseline.id()));

    // Clean up so the harness's drop doesn't trip on a stale merge.
    repo.merge_state_manager().abort().unwrap();
}

#[test]
fn snapshot_atomic_mutation_fault_and_exactly_once_contract() {
    let (temp_dir, repo) = create_test_repo();
    fs::write(temp_dir.path().join("tracked.txt"), "baseline").unwrap();
    let baseline = repo.snapshot(Some("baseline".to_string()), None).unwrap();

    fs::write(temp_dir.path().join("tracked.txt"), "pre-commit crash").unwrap();
    let crashed = with_snapshot_fault(SnapshotFault::AfterStageBeforeAtomicCommit, || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = repo.snapshot(Some("must not commit".to_string()), None);
        }))
    });
    assert!(
        crashed.is_err(),
        "the pre-commit checkpoint must crash the in-flight capture"
    );
    assert_eq!(
        repo.head().unwrap(),
        Some(baseline.id()),
        "a pre-commit crash must leave the previous capture visible"
    );

    fs::write(temp_dir.path().join("tracked.txt"), "committed once").unwrap();
    let committed_crash =
        with_snapshot_fault(SnapshotFault::AfterAtomicCommitBeforeRefPublish, || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = repo.snapshot(Some("committed exactly once".to_string()), None);
            }))
        });
    assert!(
        committed_crash.is_err(),
        "the post-commit checkpoint must crash after the oplog append"
    );
    let captured = repo
        .snapshot(Some("committed exactly once".to_string()), None)
        .unwrap();
    let recent = repo.oplog().recent(16).unwrap();
    let snapshot_batch = recent
        .iter()
        .find(|entry| {
            matches!(
                entry.operation,
                OpRecord::Snapshot { new_state, .. } if new_state == captured.id()
            )
        })
        .map(|entry| entry.batch_id)
        .expect("capture must append a snapshot record");
    let batch_entries = recent
        .iter()
        .filter(|entry| entry.batch_id == snapshot_batch)
        .collect::<Vec<_>>();
    let snapshot_count = batch_entries
        .iter()
        .filter(|entry| matches!(entry.operation, OpRecord::Snapshot { .. }))
        .count();
    let transaction_count = batch_entries
        .iter()
        .filter(|entry| matches!(entry.operation, OpRecord::TransactionCommit { .. }))
        .count();
    assert_eq!(
        snapshot_count, 1,
        "capture batch must contain one snapshot record"
    );
    assert_eq!(
        transaction_count, 1,
        "capture batch must contain one transaction marker"
    );
}

#[test]
fn test_snapshot_with_parent() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("a.txt"), "a").unwrap();
    let state1 = repo.snapshot(Some("First".to_string()), None).unwrap();

    fs::write(temp_dir.path().join("b.txt"), "b").unwrap();
    let state2 = repo.snapshot(Some("Second".to_string()), None).unwrap();

    assert_eq!(state2.parents, vec![state1.id()]);
}

#[test]
fn test_snapshot_without_confidence_records_none() {
    let temp_dir = TempDir::new().unwrap();
    Repository::init_default(temp_dir.path()).unwrap();

    let config_path = temp_dir.path().join(".heddle/config.toml");
    let mut config = RepoConfig::load(&config_path).unwrap();
    config.agent.provider = Some("test-provider".to_string());
    config.agent.model = Some("test-model".to_string());
    config.save(&config_path).unwrap();

    let repo = Repository::open(temp_dir.path()).unwrap();
    fs::write(temp_dir.path().join("agent.txt"), "content").unwrap();

    let state = repo.snapshot(None, None).unwrap();
    assert_eq!(state.confidence, None);
}

#[test]
fn test_goto_restores_state() {
    let (temp_dir, repo) = create_test_repo();

    let file_path = temp_dir.path().join("a.txt");
    fs::write(&file_path, "version 1").unwrap();
    let state1 = repo.snapshot(Some("Version 1".to_string()), None).unwrap();

    let tree1 = repo.store().get_tree(&state1.tree).unwrap().unwrap();
    assert!(
        tree1.get("a.txt").is_some(),
        "state1 should have a.txt in tree"
    );

    fs::write(&file_path, "version 2").unwrap();
    let state2 = repo.snapshot(Some("Version 2".to_string()), None).unwrap();

    let tree2 = repo.store().get_tree(&state2.tree).unwrap().unwrap();
    assert!(
        tree2.get("a.txt").is_some(),
        "state2 should have a.txt in tree"
    );
    let tree1_hash = tree1
        .get("a.txt")
        .unwrap()
        .blob_hash()
        .expect("a.txt is a blob");
    let tree2_hash = tree2
        .get("a.txt")
        .unwrap()
        .blob_hash()
        .expect("a.txt is a blob");
    assert_ne!(tree1_hash, tree2_hash);

    let blob1 = repo.store().get_blob(&tree1_hash).unwrap();
    assert!(blob1.is_some(), "blob for a.txt v1 should exist");
    assert_eq!(blob1.unwrap().content_str(), Some("version 1"));

    repo.goto(&state1.id()).unwrap();

    assert!(file_path.exists(), "a.txt should exist after goto");
    let content = fs::read_to_string(&file_path).unwrap();
    assert_eq!(content, "version 1");
}

#[test]
fn test_goto_clears_non_empty_directories() {
    let (temp_dir, repo) = create_test_repo();

    let sub_dir = temp_dir.path().join("subdir");
    fs::create_dir(&sub_dir).unwrap();
    fs::write(sub_dir.join("file.txt"), "content").unwrap();

    let state1 = repo
        .snapshot(Some("With subdir".to_string()), None)
        .unwrap();

    fs::write(temp_dir.path().join("new_file.txt"), "new").unwrap();

    repo.goto_discard_local(&state1.id()).unwrap();

    assert!(!temp_dir.path().join("new_file.txt").exists());
    assert!(temp_dir.path().join("subdir").exists());
    assert!(temp_dir.path().join("subdir/file.txt").exists());
}

#[test]
fn test_build_tree_rejects_large_files() {
    let (temp_dir, repo) = create_test_repo();

    let large_file = temp_dir.path().join("large.txt");
    let content = vec![b'a'; 101 * 1024 * 1024]; // 101 MB
    fs::write(&large_file, content).unwrap();

    let result = repo.build_tree(temp_dir.path());
    assert!(matches!(result, Err(HeddleError::InvalidFileSize(_))));
}

#[test]
fn test_compare_worktree_cached_does_not_store_modified_file_blob() {
    let (temp_dir, repo) = create_test_repo();
    let path = temp_dir.path().join("tracked.txt");

    fs::write(&path, "v1").unwrap();
    let state = repo.snapshot(Some("initial".to_string()), None).unwrap();
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();
    let blob_count_before = repo.store().list_blobs().unwrap().len();

    fs::write(&path, "v2").unwrap();

    let status = repo.compare_worktree_cached(&tree).unwrap();
    let blob_count_after = repo.store().list_blobs().unwrap().len();

    assert_eq!(
        status.modified,
        vec![std::path::PathBuf::from("tracked.txt")]
    );
    assert_eq!(status.added, Vec::<std::path::PathBuf>::new());
    assert_eq!(status.deleted, Vec::<std::path::PathBuf>::new());
    assert_eq!(blob_count_after, blob_count_before);
}

#[test]
fn test_compare_worktree_cached_does_not_store_added_file_blob() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("tracked.txt"), "v1").unwrap();
    let state = repo.snapshot(Some("initial".to_string()), None).unwrap();
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();
    let blob_count_before = repo.store().list_blobs().unwrap().len();

    fs::write(temp_dir.path().join("added.txt"), "new file").unwrap();

    let status = repo.compare_worktree_cached(&tree).unwrap();
    let blob_count_after = repo.store().list_blobs().unwrap().len();

    assert_eq!(status.added, vec![std::path::PathBuf::from("added.txt")]);
    assert_eq!(status.modified, Vec::<std::path::PathBuf>::new());
    assert_eq!(status.deleted, Vec::<std::path::PathBuf>::new());
    assert_eq!(blob_count_after, blob_count_before);
}

#[test]
fn test_compare_worktree_cached_detects_new_file_in_already_tracked_directory() {
    // Regression: a new file added to an already-tracked directory must be
    // detected even after a prior scan marked that subtree clean. The prior
    // scan populates the directory cache with a `clean_tree_hash`; a parent-level
    // whole-subtree skip keyed on that hash plus the parent's child-name digest
    // is unsound, because adding `src/c.txt` changes neither the parent's child
    // list nor the committed tree hash, so the addition would be silently missed
    // (status reports clean / capture refuses). Without an fsmonitor vouching for
    // the subtree, the scan must descend and surface the new file.
    let (temp_dir, repo) = create_test_repo();

    let tracked_dir = temp_dir.path().join("src");
    fs::create_dir(&tracked_dir).unwrap();
    fs::write(tracked_dir.join("a.txt"), "a").unwrap();
    let state = repo.snapshot(Some("base".to_string()), None).unwrap();
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();

    // First scan: populates the directory cache and marks src/ subtree clean.
    let first = repo.compare_worktree_cached(&tree).unwrap();
    assert_eq!(first.added, Vec::<std::path::PathBuf>::new());
    assert_eq!(first.modified, Vec::<std::path::PathBuf>::new());
    assert_eq!(first.deleted, Vec::<std::path::PathBuf>::new());

    // Add a new file inside the already-tracked directory. This leaves src/'s
    // entry in the committed tree (and the root's child list) unchanged.
    fs::write(tracked_dir.join("c.txt"), "c").unwrap();

    // Second scan: the new file must be detected, not skipped by the cache.
    let second = repo.compare_worktree_cached(&tree).unwrap();
    assert_eq!(
        second.added,
        vec![std::path::PathBuf::from("src/c.txt")],
        "new file in an already-tracked directory must be detected on rescan"
    );
    assert_eq!(second.modified, Vec::<std::path::PathBuf>::new());
    assert_eq!(second.deleted, Vec::<std::path::PathBuf>::new());
}

#[test]
fn test_compare_worktree_cached_marks_tracked_file_replaced_by_directory_modified() {
    let (temp_dir, repo) = create_test_repo();
    let path = temp_dir.path().join("tracked.txt");

    fs::write(&path, "v1").unwrap();
    let state = repo.snapshot(Some("initial".to_string()), None).unwrap();
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();

    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    fs::write(path.join("nested.txt"), "nested").unwrap();

    let status = repo.compare_worktree_cached(&tree).unwrap();

    assert_eq!(
        status.modified,
        vec![std::path::PathBuf::from("tracked.txt")]
    );
}

#[test]
fn test_compare_worktree_cached_reports_nested_additions_under_file_directory_collision() {
    let (temp_dir, repo) = create_test_repo();
    let path = temp_dir.path().join("tracked.txt");

    fs::write(&path, "v1").unwrap();
    let state = repo.snapshot(Some("initial".to_string()), None).unwrap();
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();

    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    fs::write(path.join("nested.txt"), "nested").unwrap();

    let status = repo.compare_worktree_cached(&tree).unwrap();

    assert_eq!(
        status.modified,
        vec![std::path::PathBuf::from("tracked.txt")]
    );
    assert_eq!(
        status.added,
        vec![std::path::PathBuf::from("tracked.txt/nested.txt")]
    );
}

#[test]
fn test_compare_worktree_cached_persists_pure_untracked_subtree_results() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("tracked.txt"), "tracked").unwrap();
    let state = repo.snapshot(Some("initial".to_string()), None).unwrap();
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();

    let untracked_dir = temp_dir.path().join("scratch/nested");
    fs::create_dir_all(&untracked_dir).unwrap();
    fs::write(untracked_dir.join("file.txt"), "scratch").unwrap();

    let first = repo.compare_worktree_cached(&tree).unwrap();
    let index_path = temp_dir.path().join(".heddle/state/index.bin");
    let index = WorktreeIndex::load(&index_path).unwrap();

    assert_eq!(
        first.added,
        vec![std::path::PathBuf::from("scratch/nested/file.txt")]
    );
    assert_eq!(
        index
            .get_untracked_directory("scratch")
            .map(|entry| entry.added_paths.clone()),
        Some(vec!["nested/file.txt".to_string()])
    );

    let second = repo.compare_worktree_cached(&tree).unwrap();
    assert_eq!(second.added, first.added);
    assert_eq!(second.modified, first.modified);
    assert_eq!(second.deleted, first.deleted);
}

#[test]
fn test_compare_worktree_cached_detailed_uses_untracked_subtrees_and_flattens_exactly() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("tracked.txt"), "tracked").unwrap();
    let state = repo.snapshot(Some("initial".to_string()), None).unwrap();
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();

    fs::create_dir_all(temp_dir.path().join("scratch/nested")).unwrap();
    fs::write(temp_dir.path().join("scratch/nested/file-a.txt"), "a").unwrap();
    fs::write(temp_dir.path().join("scratch/nested/file-b.txt"), "b").unwrap();
    fs::write(temp_dir.path().join("loose.txt"), "loose").unwrap();

    let detailed = repo.compare_worktree_cached_detailed(&tree).unwrap();
    let mut flattened = detailed.clone().into_flat_status();
    let mut exact = repo.compare_worktree_cached(&tree).unwrap();

    flattened.modified.sort();
    flattened.added.sort();
    flattened.deleted.sort();
    exact.modified.sort();
    exact.added.sort();
    exact.deleted.sort();

    assert!(detailed.modified.is_empty());
    assert!(detailed.deleted.is_empty());
    assert_eq!(
        detailed.untracked.files,
        vec![std::path::PathBuf::from("loose.txt")]
    );
    assert_eq!(detailed.untracked.subtrees.len(), 1);
    assert_eq!(
        detailed.untracked.subtrees[0].root,
        std::path::PathBuf::from("scratch")
    );
    assert_eq!(
        detailed.untracked.subtrees[0].relative_files,
        vec![
            "nested/file-a.txt".to_string(),
            "nested/file-b.txt".to_string()
        ]
    );
    assert_eq!(flattened.added, exact.added);
    assert_eq!(flattened.modified, exact.modified);
    assert_eq!(flattened.deleted, exact.deleted);
}

#[test]
#[cfg(target_os = "linux")]
fn test_build_tree_rejects_escaping_symlinks() {
    let (temp_dir, repo) = create_test_repo();

    let symlink_path = temp_dir.path().join("escape");
    let outside_dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside_dir.path(), &symlink_path).unwrap();

    let result = repo.build_tree(temp_dir.path());
    assert!(matches!(result, Err(HeddleError::InvalidSymlinkTarget(_))));
}

#[test]
#[cfg(unix)]
fn test_build_tree_allows_valid_symlinks() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("target.txt"), "target content").unwrap();
    let symlink_path = temp_dir.path().join("link");
    std::os::unix::fs::symlink("target.txt", &symlink_path).unwrap();

    let tree = repo.build_tree(temp_dir.path()).unwrap();
    assert!(tree.get("link").is_some());
}

#[test]
#[cfg(unix)]
fn test_compare_worktree_cached_marks_clean_symlink_index_entry_fresh() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("target.txt"), "target content").unwrap();
    std::os::unix::fs::symlink("target.txt", temp_dir.path().join("link")).unwrap();

    let state = repo.snapshot(Some("initial".to_string()), None).unwrap();
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();

    assert!(repo.worktree_is_clean_cached(&tree).unwrap());
    assert!(repo.worktree_is_clean_cached(&tree).unwrap());

    let index_path = temp_dir.path().join(".heddle/state/index.bin");
    let index = WorktreeIndex::load(&index_path).unwrap();
    let metadata = fs::symlink_metadata(temp_dir.path().join("link")).unwrap();

    assert!(index.is_fresh("link", &metadata));
}

#[test]
#[cfg(unix)]
fn test_compare_worktree_cached_marks_retargeted_symlink_modified() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("old.txt"), "old content").unwrap();
    fs::write(temp_dir.path().join("new.txt"), "new content").unwrap();
    std::os::unix::fs::symlink("old.txt", temp_dir.path().join("link")).unwrap();

    let state = repo.snapshot(Some("initial".to_string()), None).unwrap();
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();

    fs::remove_file(temp_dir.path().join("link")).unwrap();
    std::os::unix::fs::symlink("new.txt", temp_dir.path().join("link")).unwrap();

    let status = repo.compare_worktree_cached(&tree).unwrap();
    assert_eq!(status.modified, vec![std::path::PathBuf::from("link")]);
    assert!(
        status.added.is_empty(),
        "retargeting a tracked symlink must not classify it as added"
    );
    assert!(status.deleted.is_empty());
}

#[test]
#[cfg(unix)]
fn test_materialize_tree_creates_symlinks() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("target.txt"), "target").unwrap();
    std::os::unix::fs::symlink("target.txt", temp_dir.path().join("link")).unwrap();

    let tree = repo.build_tree(temp_dir.path()).unwrap();
    assert!(tree.get("link").is_some());

    fs::remove_file(temp_dir.path().join("link")).unwrap();
    fs::remove_file(temp_dir.path().join("target.txt")).unwrap();

    repo.materialize_tree(&tree, temp_dir.path()).unwrap();

    let link_path = temp_dir.path().join("link");
    let target_path = temp_dir.path().join("target.txt");

    // Phase E: tighten this. The pre-Phase-E version used
    // `if link_path.exists() || fs::read_link(...).is_ok()` which silently
    // passed when the symlink was actually a regular file containing the
    // target string. Assert the link is genuinely a symlink.
    let meta =
        fs::symlink_metadata(&link_path).expect("symlink path should exist after materialize");
    assert!(
        meta.file_type().is_symlink(),
        "materialized 'link' must be a real symlink, not a regular file"
    );
    let target = fs::read_link(&link_path).expect("readlink should succeed");
    assert_eq!(target, std::path::Path::new("target.txt"));
    assert!(target_path.exists());
}

#[test]
#[cfg(unix)]
fn test_materialize_tree_rejects_relative_symlink_escape() {
    let (temp_dir, repo) = create_test_repo();
    let materialize_root = temp_dir.path().join("materialized");
    let tree = handcrafted_symlink_tree(&repo, std::path::Path::new("../outside"));

    let result = repo.materialize_tree(&tree, &materialize_root);

    assert!(matches!(result, Err(HeddleError::InvalidSymlinkTarget(_))));
    assert!(
        fs::symlink_metadata(materialize_root.join("link")).is_err(),
        "escaping symlink must fail before the link is created"
    );
}

#[test]
#[cfg(unix)]
fn test_materialize_tree_rejects_normalized_relative_symlink_escape() {
    let (temp_dir, repo) = create_test_repo();
    let materialize_root = temp_dir.path().join("materialized");
    let tree = handcrafted_symlink_tree(&repo, std::path::Path::new(".heddle/../../outside"));

    let result = repo.materialize_tree(&tree, &materialize_root);

    assert!(matches!(result, Err(HeddleError::InvalidSymlinkTarget(_))));
    assert!(
        fs::symlink_metadata(materialize_root.join("link")).is_err(),
        "normalized escaping symlink must fail before the link is created"
    );
}

#[test]
#[cfg(unix)]
fn test_materialize_tree_rejects_absolute_symlink_escape() {
    let (temp_dir, repo) = create_test_repo();
    let materialize_root = temp_dir.path().join("materialized");
    let outside_target = temp_dir.path().join("outside-target");
    let tree = handcrafted_symlink_tree(&repo, &outside_target);

    let result = repo.materialize_tree(&tree, &materialize_root);

    assert!(matches!(result, Err(HeddleError::InvalidSymlinkTarget(_))));
    assert!(
        fs::symlink_metadata(materialize_root.join("link")).is_err(),
        "absolute escaping symlink must fail before the link is created"
    );
}

#[test]
#[cfg(unix)]
fn test_materialize_tree_allows_handcrafted_in_repo_symlink() {
    let (temp_dir, repo) = create_test_repo();
    let materialize_root = temp_dir.path().join("materialized");
    let tree = handcrafted_symlink_tree(&repo, std::path::Path::new("target.txt"));

    repo.materialize_tree(&tree, &materialize_root).unwrap();

    let link_path = materialize_root.join("link");
    let meta = fs::symlink_metadata(&link_path).unwrap();
    assert!(meta.file_type().is_symlink());
    assert_eq!(
        fs::read_link(&link_path).unwrap(),
        std::path::Path::new("target.txt")
    );
}

#[test]
#[cfg(unix)]
fn test_capture_and_materialize_reject_same_escaping_symlink_target() {
    let (temp_dir, repo) = create_test_repo();
    let target = std::path::Path::new("../outside");
    std::os::unix::fs::symlink(target, temp_dir.path().join("capture-link")).unwrap();

    let capture_result = repo.build_tree(temp_dir.path());
    assert!(matches!(
        capture_result,
        Err(HeddleError::InvalidSymlinkTarget(_))
    ));

    let materialize_root = temp_dir.path().join("materialized");
    let tree = handcrafted_symlink_tree(&repo, target);
    let materialize_result = repo.materialize_tree(&tree, &materialize_root);
    assert!(matches!(
        materialize_result,
        Err(HeddleError::InvalidSymlinkTarget(_))
    ));
    assert!(
        fs::symlink_metadata(materialize_root.join("link")).is_err(),
        "materialize must reject the same target capture rejects"
    );
}

#[test]
fn test_materialize_tree_restores_nested_directories() {
    let (temp_dir, repo) = create_test_repo();

    let nested_dir = temp_dir.path().join("src/bin");
    fs::create_dir_all(&nested_dir).unwrap();
    fs::write(
        temp_dir.path().join("Cargo.toml"),
        "[package]\nname='demo'\n",
    )
    .unwrap();
    fs::write(nested_dir.join("app.rs"), "fn main() {}\n").unwrap();

    let tree = repo.build_tree(temp_dir.path()).unwrap();

    fs::remove_dir_all(temp_dir.path().join("src")).unwrap();
    fs::remove_file(temp_dir.path().join("Cargo.toml")).unwrap();

    repo.materialize_tree(&tree, temp_dir.path()).unwrap();

    assert_eq!(
        fs::read_to_string(temp_dir.path().join("Cargo.toml")).unwrap(),
        "[package]\nname='demo'\n"
    );
    assert_eq!(
        fs::read_to_string(temp_dir.path().join("src/bin/app.rs")).unwrap(),
        "fn main() {}\n"
    );
}

#[test]
#[cfg(unix)]
fn test_materialize_tree_restores_executable_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let (temp_dir, repo) = create_test_repo();
    let script_path = temp_dir.path().join("script.sh");

    fs::write(&script_path, "#!/bin/sh\nexit 0\n").unwrap();
    let mut perms = fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script_path, perms).unwrap();

    let tree = repo.build_tree(temp_dir.path()).unwrap();
    fs::remove_file(&script_path).unwrap();

    repo.materialize_tree(&tree, temp_dir.path()).unwrap();

    // Two outcomes are correct depending on whether the materializer
    // hardlinks from the canonical loose blob (`0o555`, read-only
    // executable, prevents naive in-place edits from corrupting the
    // shared inode) or copies the bytes via `fs::write` fallback
    // (`0o755`, normal executable). Both preserve the *executable*
    // bit, which is what this test is here to pin. See
    // `repository_materialization::set_hardlink_mode` for the
    // hardlink-mode rationale.
    let restored_mode = fs::metadata(&script_path).unwrap().permissions().mode() & 0o777;
    assert!(
        restored_mode == 0o755 || restored_mode == 0o555,
        "expected 0o755 (fs::write fallback) or 0o555 (hardlinked, read-only-exec), got 0o{:o}",
        restored_mode
    );
    // Whichever mode wired up, the executable bit must be set.
    assert!(
        restored_mode & 0o111 != 0,
        "executable bit must survive materialization, got 0o{:o}",
        restored_mode
    );
}

#[test]
#[cfg(unix)]
fn test_build_tree_rejects_dangling_symlink_escaping_repo() {
    let (temp_dir, repo) = create_test_repo();

    let symlink_path = temp_dir.path().join("escape");
    std::os::unix::fs::symlink("/nonexistent/../../../etc/passwd", &symlink_path).unwrap();

    let result = repo.build_tree(temp_dir.path());
    assert!(
        matches!(result, Err(HeddleError::InvalidSymlinkTarget(_))),
        "Should reject dangling symlink that escapes repo via .. traversal"
    );
}

#[test]
#[cfg(unix)]
fn test_build_tree_allows_dangling_symlink_inside_repo() {
    let (temp_dir, repo) = create_test_repo();

    let symlink_path = temp_dir.path().join("link");
    std::os::unix::fs::symlink("does-not-exist.txt", &symlink_path).unwrap();

    let tree = repo.build_tree(temp_dir.path()).unwrap();
    assert!(
        tree.get("link").is_some(),
        "Should allow dangling symlink inside repo"
    );
}

#[test]
fn test_query_history_filters_by_changed_path() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("src.rs"), "one").unwrap();
    repo.snapshot(Some("add src".to_string()), None).unwrap();

    fs::write(temp_dir.path().join("docs.md"), "docs").unwrap();
    repo.snapshot(Some("add docs".to_string()), None).unwrap();

    fs::write(temp_dir.path().join("src.rs"), "two").unwrap();
    repo.snapshot(Some("update src".to_string()), None).unwrap();

    let query = HistoryQuery::new(repo.head().unwrap())
        .with_limit(10)
        .with_changed_paths(ChangedPathFilters::try_from_paths(["src.rs"]).unwrap());

    let history = repo.query_history(&query).unwrap();
    let intents: Vec<_> = history
        .iter()
        .map(|state| state.intent.as_deref().unwrap_or(""))
        .collect();

    assert_eq!(intents, vec!["update src", "add src"]);
}

#[test]
fn test_query_history_directory_filter_matches_nested_paths() {
    let (temp_dir, repo) = create_test_repo();

    fs::create_dir_all(temp_dir.path().join("src")).unwrap();
    fs::write(temp_dir.path().join("src/lib.rs"), "one").unwrap();
    repo.snapshot(Some("src change".to_string()), None).unwrap();

    fs::write(temp_dir.path().join("top.txt"), "root").unwrap();
    repo.snapshot(Some("root change".to_string()), None)
        .unwrap();

    let query = HistoryQuery::new(repo.head().unwrap())
        .with_limit(10)
        .with_changed_paths(ChangedPathFilters::try_from_paths(["src"]).unwrap());

    let history = repo.query_history(&query).unwrap();

    assert_eq!(history.len(), 1);
    assert_eq!(history[0].intent.as_deref(), Some("src change"));
}

/// Regression: `--since <state> --path <p>` used to silently degrade
/// to "no bound" when `<state>` itself was filtered out by `<p>`.
/// The fix applies `stop_at` BEFORE the path filter, so matches older
/// than the bound never leak into the result. Setup: three snapshots
/// of which the second touches `docs.md` only; bound the walk at the
/// first while filtering for `src.rs`. The first state is filtered
/// out, so under the old behavior its position was None and the bound
/// was a no-op — leaking the older `src.rs` change. With `stop_at`,
/// the walk terminates at the bound regardless of filter, so only
/// post-bound `src.rs` matches survive.
#[test]
fn test_query_history_since_with_path_filter_bounds_walk_first() {
    let (temp_dir, repo) = create_test_repo();

    // Oldest: a `src.rs` change that should NEVER appear once we
    // bound the walk at a later state.
    fs::write(temp_dir.path().join("src.rs"), "v1").unwrap();
    let s1 = repo.snapshot(Some("oldest src".to_string()), None).unwrap();

    // Bound state — touches only docs.md, so it's filtered out by
    // `--path src.rs`. This is the state that historically broke the
    // bound.
    fs::write(temp_dir.path().join("docs.md"), "doc").unwrap();
    let s2 = repo.snapshot(Some("docs only".to_string()), None).unwrap();

    // Post-bound src.rs change — must appear.
    fs::write(temp_dir.path().join("src.rs"), "v2").unwrap();
    repo.snapshot(Some("newer src".to_string()), None).unwrap();

    let query = HistoryQuery::new(repo.head().unwrap())
        .with_limit(10)
        .with_changed_paths(ChangedPathFilters::try_from_paths(["src.rs"]).unwrap())
        .with_stop_at(Some(s2.id()));

    let history = repo.query_history(&query).unwrap();
    let intents: Vec<_> = history
        .iter()
        .map(|state| state.intent.as_deref().unwrap_or(""))
        .collect();

    // Only the post-bound src.rs change should survive — the oldest
    // src.rs predates the bound and must NOT leak through.
    assert_eq!(intents, vec!["newer src"]);
    // Sanity: confirm the bounded state itself is excluded (it would
    // have been filtered anyway, but the bound is exclusive by
    // contract).
    assert!(!history.iter().any(|s| s.id() == s2.id()));
    // And confirm `s1` is excluded — that's the regression.
    assert!(!history.iter().any(|s| s.id() == s1.id()));
}

/// Same shape as the path-filter regression but for `--agent`. Bound
/// state's agent doesn't match the filter; matches older than the
/// bound used to leak through. The fix applies `stop_at` before the
/// agent filter.
#[test]
fn test_query_history_since_with_agent_filter_bounds_walk_first() {
    use objects::object::{Agent, Attribution, Principal};
    let (temp_dir, repo) = create_test_repo();

    let claude_attr = Attribution::with_agent(
        Principal::new("Tester", "test@example.com"),
        Agent::new("anthropic", "claude-opus-4"),
    );
    let codex_attr = Attribution::with_agent(
        Principal::new("Tester", "test@example.com"),
        Agent::new("openai", "codex-mini"),
    );

    // Oldest: a Claude capture that must not leak once bounded.
    fs::write(temp_dir.path().join("a.txt"), "1").unwrap();
    let s1 = repo
        .snapshot_with_attribution(Some("old claude".to_string()), None, claude_attr.clone())
        .unwrap();

    // Bound — codex agent, filtered out by `--agent claude`.
    fs::write(temp_dir.path().join("b.txt"), "1").unwrap();
    let s2 = repo
        .snapshot_with_attribution(Some("codex bound".to_string()), None, codex_attr)
        .unwrap();

    // Newer claude capture — must appear.
    fs::write(temp_dir.path().join("c.txt"), "1").unwrap();
    repo.snapshot_with_attribution(Some("new claude".to_string()), None, claude_attr)
        .unwrap();

    let query = HistoryQuery::new(repo.head().unwrap())
        .with_limit(10)
        .with_agent_filter(Some("claude".to_string()))
        .with_stop_at(Some(s2.id()));

    let history = repo.query_history(&query).unwrap();
    let intents: Vec<_> = history
        .iter()
        .map(|state| state.intent.as_deref().unwrap_or(""))
        .collect();

    assert_eq!(intents, vec!["new claude"]);
    assert!(!history.iter().any(|s| s.id() == s1.id()));
}

#[test]
fn test_performance_inspection_reports_repo_shape() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("tracked.txt"), "tracked").unwrap();
    let state = repo.snapshot(Some("tracked".to_string()), None).unwrap();
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();
    let _ = repo.compare_worktree_cached(&tree).unwrap();

    let blob_hash = objects::object::ContentHash::compute(b"missing");
    repo.record_missing_blob(blob_hash).unwrap();

    let report = repo.inspect_performance().unwrap();

    assert_eq!(report.ref_counts.threads, 1);
    assert!(report.ref_summary_index.present);
    assert!(report.ref_summary_index.valid);
    assert_eq!(report.ref_summary_index.threads, 1);
    assert!(report.worktree_index.present);
    assert!(report.worktree_index.file_entries >= 1);
    assert_eq!(report.partial_fetch.missing_blob_count, 1);
    assert_eq!(report.pull_planner_cache.status, "absent");
    assert!(!report.pull_planner_cache.present);
    assert!(!report.change_monitor.backend.is_empty());
    assert!(!report.change_monitor.status.is_empty());
}

#[test]
fn test_maintenance_run_builds_commit_graph_and_worktree_index() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("tracked.txt"), "v1").unwrap();
    repo.snapshot(Some("v1".to_string()), None).unwrap();
    fs::write(temp_dir.path().join("tracked.txt"), "v2").unwrap();
    repo.snapshot(Some("v2".to_string()), None).unwrap();

    let graph_path = temp_dir.path().join(".heddle/state/commit-graph.bin");
    let index_path = temp_dir.path().join(".heddle/state/index.bin");
    assert!(!graph_path.exists());
    assert!(!index_path.exists());

    let run = repo.run_maintenance().unwrap();

    assert!(run.rebuilt_commit_graph);
    assert!(run.rebuilt_ref_summary_index);
    assert!(run.rebuilt_worktree_index);
    assert!(graph_path.exists());
    assert!(index_path.exists());
    assert!(run.report.commit_graph.present);
    assert!(run.report.commit_graph.node_count >= 2);
    assert!(run.report.commit_graph.bloom_covered_nodes >= 1);
    assert!(run.report.ref_summary_index.present);
    assert!(run.report.ref_summary_index.valid);
    assert_eq!(run.report.ref_summary_index.threads, 1);
    assert!(run.report.worktree_index.present);
    assert!(run.report.worktree_index.file_entries >= 1);
}

#[test]
fn test_maintenance_run_prunes_and_rebuilds_pull_planner_sidecars() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("README.md"), "alpha").unwrap();
    let state = repo.snapshot(Some("alpha".to_string()), None).unwrap().id();

    let pull_root = temp_dir
        .path()
        .join(".heddle/state")
        .join("derived-summaries")
        .join("pull");
    let plans_dir = pull_root.join("plans");
    fs::create_dir_all(&plans_dir).unwrap();
    let manifest_path = pull_root.join("cold-clone-manifest.json");
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "generated_at": "2026-01-01T00:00:00Z",
            "repo_path": "org/acme/heddle",
            "head": {
                "kind": "attached",
                "value": "main",
                "head_state": state.to_string_full(),
            },
            "markers": [],
            "threads": [{
                "name": "main",
                "state_id": state.to_string_full(),
            }],
            "thread_entries": [{
                "thread": "main",
                "state_id": state.to_string_full(),
                "planner_key_full": "missing-full.json",
                "planner_key_lazy": "missing-lazy.json",
                "object_count": 0,
                "full_closure_available": true,
            }],
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(plans_dir.join("corrupt-entry.json"), b"corrupt").unwrap();
    let stale_state = crate::test_state_id();
    fs::write(
        plans_dir.join(format!(
            "{}--depth-full--exclude-af1349b9f5f9a1a6--full.json",
            stale_state.to_string_full()
        )),
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "generated_at": "2026-01-01T00:00:00Z",
            "repo_path": "org/acme/heddle",
            "remote_state_id": stale_state.to_string_full(),
            "depth": null,
            "exclude_states": [],
            "availability_mode": "Full",
            "object_count": 0,
            "planned_objects": [],
        }))
        .unwrap(),
    )
    .unwrap();

    let before = repo.inspect_performance().unwrap();
    assert!(before.pull_planner_cache.present);
    assert_eq!(before.pull_planner_cache.manifest_count, 1);
    assert_eq!(before.pull_planner_cache.planner_entry_count, 2);

    let run = repo.run_maintenance().unwrap();

    assert!(run.rebuilt_pull_planner_cache);
    assert_eq!(run.pruned_pull_planner_entries, 2);
    assert!(run.report.pull_planner_cache.present);
    assert_eq!(run.report.pull_planner_cache.manifest_count, 1);
    assert_eq!(run.report.pull_planner_cache.planner_entry_count, 2);
    assert!(manifest_path.exists());
    assert_eq!(
        fs::read_dir(plans_dir).unwrap().flatten().count(),
        2,
        "maintenance should leave only the current full and lazy planner entries"
    );
}

/// `Repository::fast_forward_attached` from inside an attached thread must
/// advance the thread's ref AND keep HEAD attached. The low-level
/// `Repository::goto` would silently detach HEAD; this unit test pins the
/// canonical helper's behavior so the merge/rebase/pull regression
/// guarantees stay enforced even if those CLI tests drift.
#[test]
fn test_fast_forward_attached_preserves_head_and_advances_thread() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("base.txt"), "base").unwrap();
    let _state1 = repo.snapshot(Some("Base".to_string()), None).unwrap();

    fs::write(temp_dir.path().join("forward.txt"), "forward").unwrap();
    let state2 = repo.snapshot(Some("Forward".to_string()), None).unwrap();

    // Repo::init_default attaches HEAD to "main"; explicitly rewind the
    // thread ref to state1 so a fast-forward to state2 is meaningful.
    let state1 = repo.head().unwrap().expect("base state should exist");
    repo.refs()
        .set_thread(&ThreadName::new("main"), &state1)
        .unwrap();
    repo.refs()
        .write_head(&Head::Attached {
            thread: ThreadName::new("main"),
        })
        .unwrap();

    repo.fast_forward_attached(&state2.id()).unwrap();

    // Thread ref must advance to the target.
    assert_eq!(
        repo.refs().get_thread(&ThreadName::new("main")).unwrap(),
        Some(state2.id()),
        "main ref must advance to fast-forward target"
    );
    // HEAD must remain attached to "main".
    let head = repo.refs().read_head().unwrap();
    assert!(
        matches!(head, Head::Attached { ref thread } if thread == "main"),
        "HEAD must remain attached to main; got {:?}",
        head
    );
    // If metadata exists for the thread, it must reflect the new state.
    // (`init_default` seeds the ref but not the Thread record; richer
    // CLI-level coverage of the metadata refresh path lives in the merge,
    // rebase, and pull regression integration tests.)
    let manager = ThreadManager::new(repo.heddle_dir());
    if let Some(meta) = manager.find_by_thread("main").unwrap() {
        assert_eq!(
            meta.current_state.as_deref(),
            Some(state2.id().short().as_str())
        );
        assert!(matches!(meta.freshness, ThreadFreshness::Current));
    }
}

/// When HEAD is detached, `fast_forward_attached` should advance the
/// worktree to the target and leave HEAD detached (no thread to advance).
#[test]
fn test_fast_forward_attached_when_detached_stays_detached() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("base.txt"), "base").unwrap();
    let state1 = repo.snapshot(Some("Base".to_string()), None).unwrap();

    fs::write(temp_dir.path().join("forward.txt"), "forward").unwrap();
    let state2 = repo.snapshot(Some("Forward".to_string()), None).unwrap();

    // Detach HEAD at state1.
    repo.goto(&state1.id()).unwrap();
    assert!(matches!(
        repo.refs().read_head().unwrap(),
        Head::Detached { .. }
    ));

    repo.fast_forward_attached(&state2.id()).unwrap();

    let head = repo.refs().read_head().unwrap();
    match head {
        Head::Detached { state } => assert_eq!(state, state2.id()),
        Head::Attached { thread } => panic!(
            "fast_forward_attached must not re-attach a previously-detached HEAD; got Attached({thread})"
        ),
    }
}

/// Regression for heddle#146: in git-overlay mode `Repository::open`
/// auto-syncs heddle's HEAD to git's branch tip. That sync MUST NOT
/// clobber an explicit `Head::Detached` written by `heddle goto`.
/// Otherwise the next `open()` (every CLI invocation reopens) silently
/// reattaches HEAD, and subsequent commands compare the worktree against
/// the wrong state — `status` reports the goto target as "dirty" and
/// `undo` refuses with "uncommitted changes".
#[test]
fn test_open_preserves_explicit_detached_head_in_git_overlay() {
    let temp_dir = TempDir::new().unwrap();
    sley::Repository::init(temp_dir.path()).expect("init real git repository");

    let repo = Repository::bootstrap_git_overlay(temp_dir.path()).unwrap();
    assert_eq!(repo.capability(), RepositoryCapability::GitOverlay);

    fs::write(temp_dir.path().join("a.txt"), "version 1").unwrap();
    let state1 = repo.snapshot(Some("v1".to_string()), None).unwrap();
    fs::write(temp_dir.path().join("a.txt"), "version 2").unwrap();
    let _state2 = repo.snapshot(Some("v2".to_string()), None).unwrap();

    repo.goto(&state1.id()).unwrap();
    assert!(
        matches!(repo.refs().read_head().unwrap(), Head::Detached { state } if state == state1.id()),
        "goto should leave HEAD detached at the target"
    );
    drop(repo);

    // Reopen: this is what every subsequent CLI invocation does. The
    // open-time git-overlay sync used to overwrite the detached HEAD
    // with `Head::Attached { thread: "main" }`.
    let reopened = Repository::open(temp_dir.path()).unwrap();
    let head = reopened.refs().read_head().unwrap();
    assert!(
        matches!(head, Head::Detached { state } if state == state1.id()),
        "reopen must preserve explicit detached HEAD; got {:?}",
        head
    );
    assert_eq!(reopened.head().unwrap(), Some(state1.id()));
}

/// Characterization: git overlay HEAD inspection matches attached/detached/unborn
/// symref semantics previously obtained from manual `.git/HEAD` parsing.
#[test]
fn git_overlay_head_state_matches_symref_semantics() {
    let temp_dir = TempDir::new().unwrap();
    let git_dir = temp_dir.path().join(".git");
    sley::Repository::init_bare(&git_dir).expect("init bare .git");
    let repo = Repository::bootstrap_git_overlay(temp_dir.path()).unwrap();

    std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    assert_eq!(
        repo.git_overlay_current_branch().unwrap(),
        Some("main".to_string())
    );
    assert!(!repo.git_overlay_head_is_detached().unwrap());

    let oid = GitObjectId::from_hex(
        GitObjectFormat::Sha1,
        "0000000000000000000000000000000000000002",
    )
    .unwrap();
    std::fs::write(git_dir.join("HEAD"), format!("{oid}\n")).unwrap();
    assert_eq!(repo.git_overlay_current_branch().unwrap(), None);
    assert!(repo.git_overlay_head_is_detached().unwrap());
    assert_eq!(
        repo.git_overlay_detached_head_commit().unwrap(),
        Some(oid.to_string())
    );

    std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/unborn\n").unwrap();
    assert_eq!(
        repo.git_overlay_current_branch().unwrap(),
        Some("unborn".to_string())
    );
}

/// Regression for heddle#152: `heddle clone <url>` produces a directory
/// whose embedded `.git/` is a bare mirror (no working tree). Shelling
/// out to `git -C <root> status --porcelain` then fails with
/// "fatal: this operation must be run in a work tree" and surfaces as
/// `Error: configuration error: git status failed at '...'` on
/// `heddle status`. The accessor must instead report "no overlay
/// status available" (`Ok(None)`) so callers fall back to heddle's own
/// tree-compare path.
#[cfg(feature = "git-overlay")]
#[test]
fn git_overlay_worktree_status_is_none_when_embedded_git_is_bare() {
    let temp_dir = TempDir::new().unwrap();
    let git_dir = temp_dir.path().join(".git");
    sley::Repository::init_bare(&git_dir).expect("init bare .git");
    let repo = Repository::bootstrap_git_overlay(temp_dir.path()).unwrap();
    assert_eq!(
        repo.capability(),
        RepositoryCapability::GitOverlay,
        "presence of .git/HEAD flips capability to GitOverlay"
    );

    let status = repo.git_overlay_worktree_status();
    assert!(
        matches!(status, Ok(None)),
        "bare embedded .git must yield Ok(None); got {}",
        match &status {
            Ok(Some(_)) => "Ok(Some(..))".to_string(),
            Ok(None) => "Ok(None)".to_string(),
            Err(error) => format!("Err({error})"),
        }
    );
}

/// Regression: `op_scope` must not embed the user's absolute filesystem
/// path. The previous behavior canonicalized HEAD to its absolute path
/// and recorded that on every oplog entry — when an oplog containing
/// those paths shipped in `examples/calculator/.heddle/`, it was a PII
/// leak for anyone who cloned the repo.
///
/// The scope must also distinguish worktrees that share one oplog
/// backend (`undo`/`redo`/`--list` filter by exact-match scope), so the
/// fix must preserve per-worktree uniqueness.
#[test]
fn test_op_scope_is_stable_unique_and_does_not_leak_absolute_path() {
    let (temp_dir, repo) = create_test_repo();
    let scope = repo.op_scope();

    // Stable across calls from the same checkout.
    assert_eq!(scope, repo.op_scope(), "op_scope must be deterministic");

    // No absolute path or home-dir leak.
    let abs_root = temp_dir.path().display().to_string();
    assert!(
        !scope.contains(&abs_root),
        "op_scope leaked absolute path: scope={scope:?} contains repo root {abs_root:?}",
    );
    assert!(
        !scope.contains('/'),
        "op_scope must not contain path separators: {scope:?}",
    );

    // Different worktrees produce different scopes — preserves
    // checkout-local undo/redo when worktrees share an oplog.
    let (other_dir, other_repo) = create_test_repo();
    assert_ne!(
        scope,
        other_repo.op_scope(),
        "different worktrees must have different op_scopes",
    );
    drop(other_dir);
}

/// `op_scope` must be invariant to the directory heddle is invoked
/// from. `Repository::open()` walks upward to find `.heddle/`, so a
/// capture run from `<root>/src/foo/` and one from `<root>` should
/// both write the same scope into the shared oplog. Otherwise
/// subdirectory invocations would record a stranger scope and break
/// undo/redo continuity from the root.
#[test]
fn test_op_scope_is_invariant_to_invocation_cwd() {
    let (temp_dir, repo_from_root) = create_test_repo();
    let nested = temp_dir.path().join("src").join("nested");
    fs::create_dir_all(&nested).unwrap();

    let repo_from_nested = Repository::open(&nested).unwrap();

    assert_eq!(
        repo_from_root.op_scope(),
        repo_from_nested.op_scope(),
        "op_scope must be cwd-invariant; opening from {nested:?} produced a different scope",
    );
}

mod blob_hydrator_callback {
    //! Read-time hydration hook (issue #50).
    //!
    //! When `Repository::require_blob` is called for a hash recorded in
    //! `partial-fetch`, the repo must invoke a registered hydrator,
    //! retry the store read, and clear the missing marker on success.
    //! On failure the underlying error must surface — partial-clone
    //! hydration is not allowed to silently degrade to "blob is just
    //! missing", which would mask network outages.
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use objects::{
        error::Result,
        object::{Blob, ContentHash},
        store::ObjectStore,
        sync::LockExt,
    };

    use super::create_test_repo;
    use crate::{BlobHydrator, HeddleError, Repository};

    /// Test double that records every call and lets the test script the
    /// outcome (success-by-write, hard error, or refuse-to-write).
    struct ScriptedHydrator {
        calls: AtomicUsize,
        seen: Mutex<Vec<ContentHash>>,
        mode: HydratorMode,
    }

    enum HydratorMode {
        /// On hydrate, write `payload` to `repo.store()` so the retry-read finds it.
        WritePayload(Vec<u8>),
        /// Return Err without writing — simulates network failure.
        Fail(String),
        /// Return Ok without writing — caller should still surface MissingObject.
        Lie,
    }

    impl ScriptedHydrator {
        fn new(mode: HydratorMode) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                seen: Mutex::new(Vec::new()),
                mode,
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn hashes_seen(&self) -> Vec<ContentHash> {
            self.seen.lock_or_poisoned().clone()
        }
    }

    impl BlobHydrator for ScriptedHydrator {
        fn hydrate(&self, repo: &Repository, hash: &ContentHash) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen.lock_or_poisoned().push(*hash);
            match &self.mode {
                HydratorMode::WritePayload(payload) => {
                    repo.store().put_blob(&Blob::new(payload.clone()))?;
                    Ok(())
                }
                HydratorMode::Fail(msg) => Err(HeddleError::Config(msg.clone())),
                HydratorMode::Lie => Ok(()),
            }
        }
    }

    #[test]
    fn require_blob_invokes_hydrator_and_clears_marker_on_success() {
        let (_temp, repo) = create_test_repo();
        let payload = b"hydrated bytes".to_vec();
        let hash = Blob::new(payload.clone()).hash();

        repo.record_missing_blob(hash).unwrap();
        assert!(
            repo.is_missing_blob(&hash).unwrap(),
            "precondition: blob must be recorded missing"
        );

        let hydrator = Arc::new(ScriptedHydrator::new(HydratorMode::WritePayload(
            payload.clone(),
        )));
        repo.set_blob_hydrator(hydrator.clone());

        let blob = repo
            .require_blob(&hash)
            .expect("require_blob must hydrate and return the blob");
        assert_eq!(blob.content(), payload.as_slice());
        assert_eq!(hydrator.call_count(), 1, "hydrator must fire exactly once");
        assert_eq!(hydrator.hashes_seen(), vec![hash]);
        assert!(
            !repo.is_missing_blob(&hash).unwrap(),
            "missing marker must be cleared after successful hydration",
        );

        // Subsequent reads must be a cache hit — hydrator stays at 1 call.
        let _ = repo.require_blob(&hash).unwrap();
        assert_eq!(
            hydrator.call_count(),
            1,
            "hydrator must not be re-invoked for a cache hit"
        );
    }

    #[test]
    fn require_blob_surfaces_hydration_error_without_silent_fallback() {
        let (_temp, repo) = create_test_repo();
        let hash = Blob::new(b"will-never-arrive".to_vec()).hash();
        repo.record_missing_blob(hash).unwrap();

        let hydrator = Arc::new(ScriptedHydrator::new(HydratorMode::Fail(
            "upstream offline".to_string(),
        )));
        repo.set_blob_hydrator(hydrator.clone());

        let err = repo
            .require_blob(&hash)
            .expect_err("require_blob must surface the hydrator error");
        let msg = err.to_string();
        assert!(
            msg.contains("upstream offline"),
            "the hydrator's error message must reach the caller verbatim; got: {msg}"
        );
        assert_eq!(hydrator.call_count(), 1);
        assert!(
            repo.is_missing_blob(&hash).unwrap(),
            "marker must remain set when hydration fails so the next attempt also tries to hydrate",
        );
    }

    #[test]
    fn require_blob_returns_missing_object_if_hydrator_lies() {
        // Defensive: if a hydrator returns Ok but doesn't actually write
        // the blob, require_blob must NOT return stale data. It must
        // raise MissingObject so the caller learns the contract was violated.
        let (_temp, repo) = create_test_repo();
        let hash = Blob::new(b"phantom-blob".to_vec()).hash();
        repo.record_missing_blob(hash).unwrap();

        let hydrator = Arc::new(ScriptedHydrator::new(HydratorMode::Lie));
        repo.set_blob_hydrator(hydrator.clone());

        let err = repo
            .require_blob(&hash)
            .expect_err("require_blob must not succeed when the blob is still absent");
        assert!(
            matches!(err, HeddleError::MissingObject { .. }),
            "expected MissingObject, got: {err:?}"
        );
        assert_eq!(hydrator.call_count(), 1);
    }

    #[test]
    fn require_blob_without_hydrator_still_returns_missing_object() {
        // Backwards-compatibility guard: callers that never registered a
        // hydrator (the common path today) must see the same
        // MissingObject error as before #50.
        let (_temp, repo) = create_test_repo();
        let hash = Blob::new(b"no-hydrator".to_vec()).hash();
        repo.record_missing_blob(hash).unwrap();

        let err = repo.require_blob(&hash).expect_err("must error");
        assert!(
            matches!(err, HeddleError::MissingObject { .. }),
            "expected MissingObject, got: {err:?}"
        );
    }

    /// Regression test for the Codex-flagged P1 (PR #53): the lazy-clone
    /// hydrator must survive a `Repository::open` boundary. Without
    /// cross-open reconstruction, `heddle clone --lazy` would work
    /// in-process but every subsequent `heddle <verb>` invocation in a
    /// fresh CLI process would see `MissingObject` because no hydrator
    /// is registered. The factory registry in
    /// [`crate::lazy_hydrator`] closes that gap; this test pins it.
    ///
    /// We register a custom test-only kind so the assertion is
    /// independent of the production git-overlay / hosted factory
    /// implementations.
    #[test]
    fn require_blob_uses_factory_registered_hydrator_after_reopen() {
        use std::path::Path;

        use crate::lazy_hydrator::{HydratorSection, LazyHydratorConfig, register_factory};

        // Test-isolated kind name — does not collide with the production
        // "git-overlay" / "hosted" kinds that other tests / CLI startup
        // register.
        const KIND: &str = "test-kind-cross-open-reopen";

        // Build a payload + its blake3 first.
        let payload = b"persisted-and-reopened".to_vec();
        let hash = Blob::new(payload.clone()).hash();

        // Set up a fresh repo and persist the lazy-hydrator metadata
        // pointing at our custom kind. Drop the repo before reopening
        // so we know the hydrator install is happening on the second
        // open, not lingering from the first construction.
        let temp = tempfile::TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let heddle_dir = repo.heddle_dir().to_path_buf();
        repo.record_missing_blob(hash).unwrap();
        // Crucially: do NOT call `set_blob_hydrator` here. The hydrator
        // must come from the factory, not from in-process state.
        let cfg = LazyHydratorConfig {
            hydrator: HydratorSection {
                kind: KIND.to_string(),
                hosted: None,
                git_overlay: None,
            },
        };
        cfg.save(&heddle_dir).unwrap();
        drop(repo);

        // Register the factory that the open path will look up. Done
        // *after* the first repo is dropped to make the order
        // explicit: open → load metadata → consult registry → install.
        let payload_for_factory = payload.clone();
        register_factory(
            KIND,
            Arc::new(
                move |_root: &Path, _section: &HydratorSection| -> Result<Arc<dyn BlobHydrator>> {
                    let bytes = payload_for_factory.clone();
                    let calls = Arc::new(AtomicUsize::new(0));
                    let calls_for_hydrator = Arc::clone(&calls);
                    Ok(Arc::new(InlineHydrator {
                        bytes,
                        calls: calls_for_hydrator,
                    }))
                },
            ),
        );

        // First reopen: the open path should pick up the metadata,
        // consult the registry, and install the factory-built hydrator.
        // require_blob then transparently hydrates.
        let reopened = Repository::open(temp.path()).unwrap();
        let blob = reopened
            .require_blob(&hash)
            .expect("hydrator must be re-installed by Repository::open");
        assert_eq!(blob.content(), payload.as_slice());
        // Marker should now be cleared after the successful hydrate.
        assert!(!reopened.is_missing_blob(&hash).unwrap());
        drop(reopened);

        // Second reopen with a *different* missing blob proves
        // reconstruction isn't a one-shot: each `Repository::open`
        // freshly installs the hydrator from the persisted metadata.
        let payload2 = b"second-reopen".to_vec();
        let hash2 = Blob::new(payload2.clone()).hash();
        // Re-register the factory under the same kind but with the
        // new payload, so the second hydrator delivers `payload2`.
        let payload2_for_factory = payload2.clone();
        register_factory(
            KIND,
            Arc::new(
                move |_root: &Path, _section: &HydratorSection| -> Result<Arc<dyn BlobHydrator>> {
                    let bytes = payload2_for_factory.clone();
                    Ok(Arc::new(InlineHydrator {
                        bytes,
                        calls: Arc::new(AtomicUsize::new(0)),
                    }))
                },
            ),
        );
        let reopened2 = Repository::open(temp.path()).unwrap();
        reopened2.record_missing_blob(hash2).unwrap();
        let blob2 = reopened2
            .require_blob(&hash2)
            .expect("second reopen must also have the hydrator installed");
        assert_eq!(blob2.content(), payload2.as_slice());
    }

    /// Minimal hydrator that writes a fixed payload on each call. Only
    /// used by `require_blob_uses_factory_registered_hydrator_after_reopen`.
    struct InlineHydrator {
        bytes: Vec<u8>,
        calls: Arc<AtomicUsize>,
    }

    impl BlobHydrator for InlineHydrator {
        fn hydrate(&self, repo: &Repository, _hash: &ContentHash) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            repo.store().put_blob(&Blob::new(self.bytes.clone()))?;
            Ok(())
        }
    }
}

mod require_tree_callback {
    //! Tree-side analog of [`super::blob_hydrator_callback`] for
    //! `Repository::require_tree` (issue heddle#93).
    //!
    //! `require_tree` has no hydrator dance — partial-fetch only ever
    //! lazy-fetches blobs, not trees — so the contract is the simpler
    //! shape: present trees round-trip; absent trees surface
    //! `MissingObject { object_type: "tree" }` with the `heddle fsck`
    //! hint baked into the display.
    //!
    //! These tests pin the contract at the API boundary; the
    //! end-to-end CLI guards in
    //! `crates/cli/tests/state_management/missing_tree_integrity.rs`
    //! cover the on-disk wiring.

    use objects::{
        object::{ContentHash, Tree},
        store::ObjectStore,
    };

    use super::create_test_repo;
    use crate::{HeddleError, Repository};

    #[test]
    fn require_tree_returns_tree_when_present_in_store() {
        let (_temp, repo): (_, Repository) = create_test_repo();
        let tree = Tree::new();
        let hash = repo.store().put_tree(&tree).unwrap();
        let loaded = repo
            .require_tree(&hash)
            .expect("require_tree must return a tree that was just put");
        assert_eq!(loaded.hash(), hash);
    }

    #[test]
    fn require_tree_returns_missing_object_when_absent() {
        let (_temp, repo): (_, Repository) = create_test_repo();
        // Use a hash that cannot collide with anything `init_default`
        // seeded: `Tree::new().hash()` was the prior choice, but
        // `init_default` seeds the empty tree, so `has_tree(hash)`
        // returned true and an early-return short-circuited the test
        // before `require_tree` was ever called (Codex r2 P3). A
        // synthetic all-`0xab` digest has no preimage and is
        // guaranteed absent from any freshly-initialised store.
        let hash = ContentHash::from_bytes([0xab; 32]);
        assert!(
            !repo.store().has_tree(&hash).unwrap(),
            "synthetic phantom hash must be absent from a fresh store",
        );

        let err = repo
            .require_tree(&hash)
            .expect_err("require_tree must error when the tree is absent from the store");
        match err {
            HeddleError::MissingObject { object_type, id } => {
                assert_eq!(
                    object_type, "tree",
                    "object_type must distinguish tree from blob"
                );
                assert_eq!(
                    id,
                    hash.to_hex(),
                    "missing-object error must carry the hash so the operator can correlate \
                     with fsck output",
                );
            }
            other => panic!("expected MissingObject, got: {other:?}"),
        }
    }

    #[test]
    fn require_tree_display_includes_fsck_recovery_hint() {
        // The hint travels with the variant's Display impl, so every
        // call site that bubbles the error to the user (via anyhow
        // `?`) gets the next-step pointer for free — no per-site
        // wrapping required.
        let err = HeddleError::MissingObject {
            object_type: "tree".to_string(),
            id: "deadbeef".to_string(),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("heddle fsck"),
            "missing-object display must include the fsck recovery hint; got: {msg}",
        );
        assert!(
            msg.contains("tree"),
            "display must carry the object_type so the operator knows what's missing; got: {msg}",
        );
        assert!(
            msg.contains("deadbeef"),
            "display must carry the id so the operator can correlate with fsck output; got: {msg}",
        );
    }
}

/// heddle#303 (AC: "a symlinked deps dir doesn't silently block
/// `ready`"). In a native Heddle repo, a `node_modules` *symlink* —
/// the workaround people reach for when an isolated checkout has no
/// installed deps — must be covered by a `node_modules/` (dir-only)
/// ignore rule, exactly as a real `node_modules/` directory would be.
/// Otherwise the symlink shows up as an uncaptured path and silently
/// blocks `ready`. The cached worktree compare is the path `ready`
/// consumes via `worktree_dirty`/`worktree_dirty_paths`.
#[cfg(unix)]
#[test]
fn dir_only_ignore_covers_node_modules_symlink_native() {
    use std::os::unix::fs::symlink;

    let (temp, repo) = create_test_repo();
    let root = temp.path();

    let real_deps = root.join("real_deps");
    fs::create_dir(&real_deps).unwrap();
    fs::write(real_deps.join("pkg.json"), "{}").unwrap();
    symlink(&real_deps, root.join("node_modules")).unwrap();
    fs::write(root.join("keep.txt"), "hi").unwrap();
    // Only the symlink basename is ignored; the link target is left
    // un-ignored so the assertion proves it's the `node_modules/` rule
    // (matching the bare symlink entry) doing the work, not collateral.
    fs::write(root.join(".heddleignore"), "node_modules/\n").unwrap();

    let state = repo.current_state().unwrap().unwrap();
    let tree = repo.require_tree(&state.tree).unwrap();
    let status = repo.compare_worktree_cached(&tree).unwrap();

    assert!(
        !status
            .added
            .iter()
            .any(|p| p == std::path::Path::new("node_modules")),
        "node_modules symlink must be ignored by `node_modules/`, not reported as added: {:?}",
        status.added,
    );
    assert!(
        status
            .added
            .iter()
            .any(|p| p == std::path::Path::new("keep.txt")),
        "a non-ignored sibling must still be reported (proves the scan ran): {:?}",
        status.added,
    );
}

/// heddle#303 (AC: "a mid-session ignore broadening takes effect
/// without `unlink`"). In a native repo, broadening `.heddleignore` to
/// `node_modules` mid-session must retroactively mask a
/// previously-seen *untracked* `node_modules/` tree on the very next
/// status, with no `unlink`/removal. Guards against a stale first-seen
/// cache decision surviving an ignore-set change.
#[cfg(unix)]
#[test]
fn midsession_ignore_broadening_masks_untracked_without_unlink_native() {
    let (temp, repo) = create_test_repo();
    let root = temp.path();

    let node_modules = root.join("node_modules");
    fs::create_dir(&node_modules).unwrap();
    fs::write(node_modules.join("dep.js"), "x").unwrap();
    fs::write(root.join("keep.txt"), "hi").unwrap();

    let state = repo.current_state().unwrap().unwrap();
    let tree = repo.require_tree(&state.tree).unwrap();

    // First status: no ignore yet, so the dep file is seen as untracked.
    let before = repo.compare_worktree_cached(&tree).unwrap();
    assert!(
        before
            .added
            .iter()
            .any(|p| p == std::path::Path::new("node_modules/dep.js")),
        "precondition: node_modules/dep.js should be untracked before the ignore: {:?}",
        before.added,
    );

    // Broaden the ignore mid-session — no removal of the path.
    fs::write(root.join(".heddleignore"), "node_modules\n").unwrap();

    let after = repo.compare_worktree_cached(&tree).unwrap();
    assert!(
        !after.added.iter().any(|p| p.starts_with("node_modules")),
        "broadened ignore must mask the previously-seen node_modules tree without unlink: {:?}",
        after.added,
    );
    assert!(
        after
            .added
            .iter()
            .any(|p| p == std::path::Path::new("keep.txt")),
        "non-ignored sibling must still be reported after the refresh: {:?}",
        after.added,
    );
}

/// heddle#303, git-overlay variant of the symlink AC. The dogfood that
/// surfaced this ran on a Git repo, so `ready` consumed
/// `git_overlay_worktree_status`. A `node_modules` symlink with a
/// `node_modules/` rule in `.gitignore` must be ignored there too.
#[cfg(all(unix, feature = "git-overlay"))]
#[test]
fn dir_only_ignore_covers_node_modules_symlink_git_overlay() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().unwrap();
    let root = temp.path();
    sley::Repository::init(root).expect("init real git repository");
    let repo = Repository::bootstrap_git_overlay(root).unwrap();
    assert_eq!(repo.capability(), RepositoryCapability::GitOverlay);

    let real_deps = root.join("real_deps");
    fs::create_dir(&real_deps).unwrap();
    fs::write(real_deps.join("pkg.json"), "{}").unwrap();
    symlink(&real_deps, root.join("node_modules")).unwrap();
    fs::write(root.join("keep.txt"), "hi").unwrap();
    fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();

    let status = repo.git_overlay_worktree_status().unwrap().unwrap();
    assert!(
        !status
            .added
            .iter()
            .any(|p| p == std::path::Path::new("node_modules")),
        "node_modules symlink must be ignored in git-overlay status: {:?}",
        status.added,
    );
    assert!(
        status
            .added
            .iter()
            .any(|p| p == std::path::Path::new("keep.txt")),
        "a non-ignored sibling must still be reported in git-overlay status: {:?}",
        status.added,
    );
}

/// heddle#303, git-overlay variant of the mid-session-refresh AC.
/// Broadening `.gitignore` to `node_modules` mid-session must mask a
/// previously-seen untracked tree on the next status, no `unlink`.
#[cfg(all(unix, feature = "git-overlay"))]
#[test]
fn midsession_ignore_broadening_masks_untracked_without_unlink_git_overlay() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    sley::Repository::init(root).expect("init real git repository");
    let repo = Repository::bootstrap_git_overlay(root).unwrap();
    assert_eq!(repo.capability(), RepositoryCapability::GitOverlay);

    let node_modules = root.join("node_modules");
    fs::create_dir(&node_modules).unwrap();
    fs::write(node_modules.join("dep.js"), "x").unwrap();
    fs::write(root.join("keep.txt"), "hi").unwrap();

    let before = repo.git_overlay_worktree_status().unwrap().unwrap();
    assert!(
        before
            .added
            .iter()
            .any(|p| p == std::path::Path::new("node_modules/dep.js")),
        "precondition: node_modules/dep.js should be untracked before the ignore: {:?}",
        before.added,
    );

    fs::write(root.join(".gitignore"), "node_modules\n").unwrap();

    let after = repo.git_overlay_worktree_status().unwrap().unwrap();
    assert!(
        !after.added.iter().any(|p| p.starts_with("node_modules")),
        "broadened .gitignore must mask the node_modules tree without unlink: {:?}",
        after.added,
    );
}

/// heddle#572 r2: a virtualized thread mounts at
/// `.heddle/threads/<encoded>/<repo-name>` with no checkout metadata of its own.
/// `Repository::open` from inside such a mount must REFUSE rather than climb
/// past the metadata-less mount and open the PARENT repo (which would apply
/// status/capture/thread operations to the wrong checkout). Solid/materialized
/// checkout roots carry their own `.heddle` pointer and are unaffected.
#[test]
fn open_refuses_metadataless_virtualized_thread_mount() {
    let (_temp, repo) = create_test_repo();

    // Simulate a virtualized thread `virt`: its mount root exists but has no
    // `.heddle` checkout metadata of its own.
    let mount_root = repo.managed_checkout_path("virt");
    fs::create_dir_all(&mount_root).unwrap();

    // `Repository` isn't `Debug`, so match rather than `expect_err`.
    let err = match Repository::open(&mount_root) {
        Ok(_) => {
            panic!("opening from a metadata-less virtualized mount must refuse the parent climb")
        }
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("virtualized thread mount"),
        "unexpected error opening the virtualized mount root: {err}"
    );

    // A path deeper inside the mount is refused too (the upward walk catches
    // the mount root as an ancestor).
    let deeper = mount_root.join("src");
    fs::create_dir_all(&deeper).unwrap();
    assert!(
        Repository::open(&deeper).is_err(),
        "a path inside the virtualized mount must also refuse"
    );

    // The detection is narrow: a solid/materialized checkout root carries its
    // own `.heddle`, so it is NOT treated as a metadata-less mount.
    let solid_root = repo.managed_checkout_path("solid");
    fs::create_dir_all(solid_root.join(".heddle")).unwrap();
    assert!(
        super::metadataless_managed_thread_root(&solid_root).is_none(),
        "a checkout root with its own .heddle must not be flagged"
    );
    assert!(
        super::metadataless_managed_thread_root(&mount_root).is_some(),
        "a metadata-less mount root must be flagged"
    );

    // Normal open from the repository root still works — the guard is narrow.
    assert!(
        Repository::open(repo.root()).is_ok(),
        "opening the repository root must still succeed"
    );
}

#[test]
fn managed_checkout_path_uses_source_repo_name_from_custom_checkout() {
    let temp = tempfile::tempdir().unwrap();
    let source_root = temp.path().join("source-repo");
    let repo = Repository::init_default(&source_root).unwrap();
    let custom_checkout = temp.path().join("custom-agent");

    Repository::init_worktree(&custom_checkout, repo.heddle_dir()).unwrap();
    let opened = Repository::open(&custom_checkout).unwrap();
    let shared_heddle = repo.heddle_dir().canonicalize().unwrap();

    assert_eq!(
        opened.managed_checkout_path("child"),
        shared_heddle
            .join("threads")
            .join("child")
            .join("source-repo"),
        "managed child threads should keep the original repo directory name, not the current checkout leaf"
    );
}

#[test]
fn open_solid_checkout_without_git_uses_native_checkout_authority() {
    let temp_dir = TempDir::new().unwrap();
    sley::Repository::init(temp_dir.path()).expect("init real git repository");
    let repo = Repository::bootstrap_git_overlay(temp_dir.path()).unwrap();
    assert_eq!(
        repo.capability(),
        RepositoryCapability::GitOverlay,
        "the parent repo is a git-overlay repo"
    );
    let heddle = repo.heddle_dir().to_path_buf();

    // Mimic write_isolated_checkout for a solid thread `feature`.
    let checkout = repo.managed_checkout_path("feature");
    let co_heddle = checkout.join(".heddle");
    fs::create_dir_all(&co_heddle).unwrap();
    fs::write(
        co_heddle.join("objectstore"),
        format!(
            "objectstore: {}\nsource-authority: native\n",
            heddle.display()
        ),
    )
    .unwrap();
    fs::create_dir_all(co_heddle.join("state")).unwrap();
    fs::write(co_heddle.join("HEAD"), "ref: feature\n").unwrap();

    let opened = Repository::open(&checkout).expect("open solid checkout");

    assert_eq!(
        opened.capability(),
        RepositoryCapability::NativeHeddle,
        "a native checkout pointer uses Heddle worktree semantics"
    );
    assert_eq!(
        opened.source_authority(),
        RepositorySourceAuthority::Native,
        "the checkout must not route source mutations into the parent Git worktree"
    );
    let reopened_parent = Repository::open(temp_dir.path()).expect("reopen parent repository");
    assert_eq!(
        reopened_parent.source_authority(),
        RepositorySourceAuthority::GitOverlay,
        "the checkout-local authority must not change the parent repository"
    );
    assert_eq!(
        opened.root(),
        checkout.canonicalize().unwrap().as_path(),
        "open must root AT the checkout"
    );
    assert!(
        matches!(opened.head_ref().unwrap(), Head::Attached { thread } if thread.as_str() == "feature"),
        "HEAD must resolve to the thread, not the parent branch"
    );
    assert_eq!(opened.current_lane().unwrap().as_deref(), Some("feature"));
}

#[test]
fn worktree_pointer_authority_controls_checkout_capability() {
    let temp_dir = TempDir::new().unwrap();
    let repo = Repository::init(temp_dir.path()).unwrap();
    let checkout = temp_dir.path().join("git-backed-worktree");
    let checkout_heddle = checkout.join(".heddle");
    fs::create_dir_all(checkout_heddle.join("state")).unwrap();
    fs::write(
        checkout_heddle.join("objectstore"),
        format!(
            "objectstore: {}\nsource-authority: git-overlay\n",
            repo.heddle_dir().display()
        ),
    )
    .unwrap();
    fs::write(checkout_heddle.join("HEAD"), "ref: main\n").unwrap();

    let opened = Repository::open(&checkout).unwrap();
    assert_eq!(opened.capability(), RepositoryCapability::GitOverlay);
    assert_eq!(
        opened.source_authority(),
        RepositorySourceAuthority::GitOverlay
    );
}

#[test]
fn source_authority_transition_compares_against_disk() {
    let temp_dir = TempDir::new().unwrap();
    sley::Repository::init(temp_dir.path()).expect("init real git repository");
    let first = Repository::bootstrap_git_overlay(temp_dir.path()).unwrap();
    let stale = Repository::open(temp_dir.path()).unwrap();

    first
        .transition_source_authority(
            RepositorySourceAuthority::GitOverlay,
            RepositorySourceAuthority::Native,
        )
        .unwrap();

    let error = stale
        .transition_source_authority(
            RepositorySourceAuthority::GitOverlay,
            RepositorySourceAuthority::Native,
        )
        .expect_err("a stale handle must not repeat the authority transition");
    assert!(
        error
            .to_string()
            .contains("expected GitOverlay, found Native")
    );

    let reopened = Repository::open(temp_dir.path()).unwrap();
    assert_eq!(
        reopened.source_authority(),
        RepositorySourceAuthority::Native
    );
    assert_eq!(reopened.capability(), RepositoryCapability::NativeHeddle);
}
