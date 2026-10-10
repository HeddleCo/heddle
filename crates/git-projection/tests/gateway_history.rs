// SPDX-License-Identifier: Apache-2.0
//! Real Git clients over complete, explicitly admitted local native histories.
//! Transport/publication authority is the host's responsibility, not this fixture.
use heddle_git_projection::{
    SyncMapping,
    gateway_view::{HistoryTip, ViewLimits, export_public_git_history, export_public_native_view},
    git_export::export_tree,
};
use objects::{
    object::{
        Attribution, ContentHash, Principal, Redaction, State, StateId, StateVisibility,
        TreeEntryTarget, VisibilityTier,
        thread_replication::{
            git_import_converter::{GitImportGraph, GitImportRawCommit},
            git_import_graph::{GitObjectFormat, GitObjectId},
            initial_base::synthetic_initial_base,
        },
    },
    store::ObjectStore,
};
use repo::Repository;
use sley::{GitObjectType, ObjectId, Repository as GitRepository};
use std::{collections::BTreeSet, path::Path, process::Command};

fn actor() -> Attribution {
    Attribution::human(Principal::new("History Fixture", "history@example.invalid"))
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Git Fixture")
        .env("GIT_AUTHOR_EMAIL", "git@example.invalid")
        .env("GIT_COMMITTER_NAME", "Git Fixture")
        .env("GIT_COMMITTER_EMAIL", "git@example.invalid")
        .env("GIT_TERMINAL_PROMPT", "0")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("Git client");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("Git ASCII output")
        .trim()
        .to_owned()
}

fn tree(repo: &Repository, files: &[(&str, &[u8])]) -> ContentHash {
    let temp = tempfile::tempdir().expect("tree fixture");
    for (name, bytes) in files {
        std::fs::write(temp.path().join(name), bytes).expect("fixture file");
    }
    let tree = repo.build_tree(temp.path()).expect("native tree");
    repo.store().put_tree(&tree).expect("store tree")
}

struct History {
    _temp: tempfile::TempDir,
    repo: Repository,
    base: StateId,
    left: StateId,
    right: StateId,
    merged: StateId,
}

fn fixture() -> History {
    let temp = tempfile::tempdir().expect("native fixture");
    let repo = Repository::init_default(temp.path()).expect("native repository");
    std::fs::write(temp.path().join("common.txt"), b"common\n").expect("base file");
    let base = repo
        .snapshot_with_attribution(Some("base".into()), None, actor())
        .expect("base capture")
        .state_id;
    let source = repo
        .create_native_thread("feature", base, Some("main"), "feature")
        .expect("source fork");
    repo.create_native_thread("target", base, Some("main"), "target")
        .expect("target fork");
    let left = State::new_snapshot(
        tree(
            &repo,
            &[("common.txt", b"common\n"), ("left.txt", b"left\n")],
        ),
        vec![base],
        actor(),
    )
    .with_intent("left");
    let right = State::new_snapshot(
        tree(
            &repo,
            &[("common.txt", b"common\n"), ("right.txt", b"right\n")],
        ),
        vec![base],
        actor(),
    )
    .with_intent("right");
    repo.put_authored_state(&left).expect("left state");
    repo.put_authored_state(&right).expect("right state");
    let operation = repo
        .record_native_capture("feature", left.id())
        .expect("left capture");
    repo.record_native_capture("target", right.id())
        .expect("right capture");
    let merged = State::new_merge(
        tree(
            &repo,
            &[
                ("common.txt", b"common\n"),
                ("left.txt", b"left\n"),
                ("right.txt", b"right\n"),
            ],
        ),
        vec![right.id(), left.id()],
        actor(),
    )
    .with_intent("native integration");
    repo.put_authored_state(&merged).expect("integration state");
    repo.record_native_local_integration(
        "target",
        merged.id(),
        source.thread_id(),
        operation,
        left.id(),
    )
    .expect("signed integration");
    History {
        _temp: temp,
        repo,
        base,
        left: left.id(),
        right: right.id(),
        merged: merged.id(),
    }
}

fn project(history: &History, path: &Path, main: StateId) -> SyncMapping {
    let sink = GitRepository::init_bare(path).expect("fresh sink");
    let mapping = export_public_git_history(
        &history.repo,
        &sink,
        &[
            HistoryTip {
                thread: "target",
                state: main,
            },
            HistoryTip {
                thread: "feature",
                state: history.left,
            },
        ],
        &["main", "target", "feature"],
        ViewLimits::default(),
    )
    .expect("complete history");
    drop(sink);
    git(
        path,
        &[
            "update-ref",
            "refs/heads/main",
            &mapping.get_git(&main).expect("main").to_string(),
        ],
    );
    git(
        path,
        &[
            "update-ref",
            "refs/heads/feature",
            &mapping.get_git(&history.left).expect("feature").to_string(),
        ],
    );
    git(path, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(path, &["fsck", "--strict", "--no-reflogs"]);
    mapping
}

#[test]
fn clone_branch_merge_fetch_and_pull_preserve_full_ordered_history() {
    let history = fixture();
    let out = tempfile::tempdir().expect("Git workspace");
    let remote = out.path().join("remote.git");
    let before = project(&history, &remote, history.right);
    assert_eq!(
        before.iter().count(),
        3,
        "base and both authored branches; no seed"
    );
    assert!(
        before
            .get_git(&synthetic_initial_base().expect("seed").id())
            .is_none()
    );
    git(
        out.path(),
        &[
            "clone",
            "--no-local",
            remote.to_str().expect("remote"),
            "reader",
        ],
    );
    git(
        out.path(),
        &[
            "clone",
            "--no-local",
            remote.to_str().expect("remote"),
            "writer",
        ],
    );
    let reader = out.path().join("reader");
    let writer = out.path().join("writer");
    assert_eq!(git(&reader, &["rev-list", "--all", "--count"]), "3");
    assert_eq!(git(&reader, &["log", "--format=%s", "main"]), "right\nbase");
    git(&writer, &["switch", "-c", "local-merge"]);
    git(
        &writer,
        &[
            "merge",
            "--no-ff",
            "-m",
            "ordinary Git merge",
            "origin/feature",
        ],
    );
    let right = before.get_git(&history.right).expect("right").to_string();
    let left = before.get_git(&history.left).expect("left").to_string();
    assert_eq!(
        git(&writer, &["show", "-s", "--format=%P", "HEAD"]),
        format!("{right} {left}")
    );
    assert_eq!(
        std::fs::read(writer.join("left.txt")).expect("left"),
        b"left\n"
    );
    assert_eq!(
        std::fs::read(writer.join("right.txt")).expect("right"),
        b"right\n"
    );

    // Build a fresh immutable generation; keep no Git object warehouse between them.
    let next = out.path().join("next.git");
    let after = project(&history, &next, history.merged);
    for (state, oid) in before.iter() {
        assert_eq!(after.get_git(state), Some(*oid));
    }
    assert_eq!(after.iter().count(), 4);
    std::fs::rename(&remote, out.path().join("old.git")).expect("retire fixture");
    std::fs::rename(&next, &remote).expect("publish new fixture");
    git(&reader, &["fetch", "origin"]);
    assert_eq!(git(&reader, &["rev-parse", "main"]), right);
    assert_eq!(
        git(&reader, &["rev-parse", "origin/main"]),
        after.get_git(&history.merged).expect("merge").to_string()
    );
    git(&reader, &["pull", "--ff-only"]);
    assert_eq!(
        git(&reader, &["show", "-s", "--format=%P", "HEAD"]),
        format!("{right} {left}")
    );
    assert_eq!(git(&reader, &["rev-list", "--all", "--count"]), "4");
    git(&reader, &["fsck", "--strict"]);
    assert_eq!(
        git(&reader, &["merge-base", "main", "origin/feature"]),
        left
    );
    let inventory: BTreeSet<_> = git(
        &remote,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    )
    .lines()
    .map(str::to_owned)
    .collect();
    let reachable: BTreeSet<_> = git(&remote, &["rev-list", "--objects", "--all"])
        .lines()
        .map(|line| line.split_whitespace().next().expect("object").to_owned())
        .collect();
    assert_eq!(
        inventory, reachable,
        "no unselected or synthetic dangling objects"
    );
}

#[test]
fn union_budget_deduplicates_ancestors_and_refuses_before_any_write() {
    let history = fixture();
    for states in [3, 4] {
        let out = tempfile::tempdir().expect("sink");
        let sink = GitRepository::init_bare(out.path()).expect("Git");
        let result = export_public_git_history(
            &history.repo,
            &sink,
            &[
                HistoryTip {
                    thread: "target",
                    state: history.right,
                },
                HistoryTip {
                    thread: "feature",
                    state: history.left,
                },
            ],
            &["main", "target", "feature"],
            ViewLimits {
                states,
                ..ViewLimits::default()
            },
        );
        if states == 4 {
            assert_eq!(result.expect("shared base counted once").iter().count(), 3);
        } else {
            assert!(
                result
                    .expect_err("bounded complete closure")
                    .to_string()
                    .contains("state limit")
            );
            assert!(
                git(
                    out.path(),
                    &["cat-file", "--batch-all-objects", "--batch-check"]
                )
                .is_empty()
            );
        }
    }
}

#[test]
fn unselected_private_branch_is_never_copied_and_selected_private_tip_denies_union() {
    let history = fixture();
    let hidden = State::new_snapshot(
        tree(&history.repo, &[("private.txt", b"secret not selected\n")]),
        vec![history.base],
        actor(),
    )
    .with_intent("hidden");
    history
        .repo
        .create_native_thread("hidden", history.base, Some("main"), "private")
        .expect("hidden fork");
    history
        .repo
        .put_authored_state(&hidden)
        .expect("hidden state");
    history
        .repo
        .put_state_visibility(StateVisibility {
            state: hidden.id(),
            tier: VisibilityTier::Internal,
            embargo_until: None,
            declarer: actor().principal,
            declared_at: chrono::Utc::now(),
            signature: None,
            supersedes: None,
        })
        .expect("private");
    history
        .repo
        .record_native_capture("hidden", hidden.id())
        .expect("private capture");
    let out = tempfile::tempdir().expect("sink");
    let path = out.path().join("public.git");
    let mapping = project(&history, &path, history.merged);
    assert!(mapping.get_git(&hidden.id()).is_none());
    let secret_oid =
        sley_core::object_id_for_bytes(sley::ObjectFormat::Sha1, "blob", b"secret not selected\n")
            .expect("hash");
    assert!(
        GitRepository::open(&path)
            .expect("sink")
            .read_object(&secret_oid)
            .is_err()
    );
    let path = out.path().join("denied.git");
    let sink = GitRepository::init_bare(&path).expect("sink");
    export_public_git_history(
        &history.repo,
        &sink,
        &[
            HistoryTip {
                thread: "target",
                state: history.merged,
            },
            HistoryTip {
                thread: "hidden",
                state: hidden.id(),
            },
        ],
        &["main", "target", "feature", "hidden"],
        ViewLimits::default(),
    )
    .expect_err("all selected refs must pass");
    assert!(git(&path, &["cat-file", "--batch-all-objects", "--batch-check"]).is_empty());
}

fn imported(repo: &Repository, base: StateId, lossy: bool) -> (State, ObjectId, Vec<u8>) {
    let temp = tempfile::tempdir().expect("conversion scratch");
    let sink = GitRepository::init_bare(temp.path()).expect("Git");
    let base_oid =
        export_public_native_view(repo, &sink, base, "main", false, ViewLimits::default())
            .expect("native base");
    let native = repo.store().get_state(&base).expect("get").expect("state");
    let tree_oid = export_tree(repo, &sink, &native.tree).expect("Git tree");
    let mut raw = format!("tree {tree_oid}\nparent {base_oid}\nauthor Alice <alice@example.invalid> 1700000000 -0700\ncommitter Bob <bob@example.invalid> 1700000020 -0000\nx-review fixture\n continuation\n\nraw imported ").into_bytes();
    raw.extend_from_slice(b"\xe9 message without newline");
    let oid = sink
        .write_raw_object(GitObjectType::Commit, raw.clone())
        .expect("Git commit");
    let git_oid = GitObjectId::Sha1(oid.as_bytes().try_into().expect("SHA1"));
    let state = GitImportGraph::convert_raw_commit(
        GitImportRawCommit {
            oid: &git_oid,
            object_format: GitObjectFormat::Sha1,
            raw_commit: &raw,
            heddle_note: None,
        },
        native.tree,
        vec![base],
        lossy,
        |_| Ok(None),
    )
    .expect("canonical importer");
    (state, oid, raw)
}

#[test]
fn signed_byte_faithful_import_reconstructs_exact_oid_without_source_git_storage() {
    let history = fixture();
    let (state, expected, raw) = imported(&history.repo, history.base, false);
    history
        .repo
        .put_authored_state(&state)
        .expect("native state");
    history
        .repo
        .record_native_capture("main", state.id())
        .expect("signed capture");
    let reloaded = Repository::open(history.repo.root()).expect("fresh native reader");
    let out = tempfile::tempdir().expect("sink");
    let sink = GitRepository::init_bare(out.path()).expect("Git");
    let mapping = export_public_git_history(
        &reloaded,
        &sink,
        &[HistoryTip {
            thread: "main",
            state: state.id(),
        }],
        &["main"],
        ViewLimits::default(),
    )
    .expect("admitted imported history");
    assert_eq!(mapping.get_git(&state.id()), Some(expected));
    assert_eq!(sink.read_object(&expected).expect("commit").body, raw);
    assert_eq!(mapping.iter().count(), 2);
    let strict = tempfile::tempdir().expect("strict sink");
    let strict = GitRepository::init_bare(strict.path()).expect("strict");
    for snapshot in [true, false] {
        assert!(
            export_public_native_view(
                &history.repo,
                &strict,
                state.id(),
                "main",
                snapshot,
                ViewLimits::default()
            )
            .is_err()
        );
    }
}

#[test]
fn imported_state_requires_signed_admission_and_original_lossy_flag_cannot_be_cleared() {
    for signed in [false, true] {
        let history = fixture();
        let (mut state, _, _) = imported(&history.repo, history.base, signed);
        history
            .repo
            .put_authored_state(&state)
            .expect("native state");
        if signed {
            history
                .repo
                .record_native_capture("main", state.id())
                .expect("signed lossy capture");
            state.git_lossy = false;
            history
                .repo
                .store()
                .put_state(&state)
                .expect("replace unhashed flag");
        }
        let out = tempfile::tempdir().expect("sink");
        let sink = GitRepository::init_bare(out.path()).expect("Git");
        let error = export_public_git_history(
            &history.repo,
            &sink,
            &[HistoryTip {
                thread: "main",
                state: state.id(),
            }],
            &["main"],
            ViewLimits::default(),
        )
        .expect_err("signed canonical fidelity required");
        assert!(
            error.to_string().contains(if signed {
                "signed source differs"
            } else {
                "no admitted native original"
            }),
            "{error}"
        );
        assert!(
            git(
                out.path(),
                &["cat-file", "--batch-all-objects", "--batch-check"]
            )
            .is_empty()
        );
    }
}

#[test]
fn git_history_refuses_redaction_instead_of_changing_existing_commit_identity() {
    let history = fixture();
    let base = history
        .repo
        .store()
        .get_state(&history.base)
        .expect("state")
        .expect("base");
    let tree = history
        .repo
        .store()
        .get_tree(&base.tree)
        .expect("tree")
        .expect("tree");
    let TreeEntryTarget::Blob { hash, .. } = tree.get("common.txt").expect("file").target() else {
        panic!("blob")
    };
    history
        .repo
        .put_redaction(Redaction {
            redacted_blob: *hash,
            state: history.base,
            path: "common.txt".into(),
            reason: "test redaction".into(),
            redactor: actor().principal,
            redacted_at: chrono::Utc::now(),
            signature: None,
            purge: None,
            supersedes: None,
        })
        .expect("redaction");
    let out = tempfile::tempdir().expect("sink");
    let sink = GitRepository::init_bare(out.path()).expect("Git");
    let error = export_public_git_history(
        &history.repo,
        &sink,
        &[HistoryTip {
            thread: "target",
            state: history.merged,
        }],
        &["main", "target", "feature"],
        ViewLimits::default(),
    )
    .expect_err("no silent rewrite");
    assert!(error.to_string().contains("redacted history"), "{error}");
    assert!(
        git(
            out.path(),
            &["cat-file", "--batch-all-objects", "--batch-check"]
        )
        .is_empty()
    );
}

#[test]
fn reordered_ref_selection_has_the_same_mapping_and_seed_only_is_not_history() {
    let history = fixture();
    let mut mappings = Vec::new();
    for tips in [
        [
            HistoryTip {
                thread: "target",
                state: history.merged,
            },
            HistoryTip {
                thread: "feature",
                state: history.left,
            },
        ],
        [
            HistoryTip {
                thread: "feature",
                state: history.left,
            },
            HistoryTip {
                thread: "target",
                state: history.merged,
            },
        ],
    ] {
        let out = tempfile::tempdir().expect("sink");
        let sink = GitRepository::init_bare(out.path()).expect("Git");
        mappings.push(
            export_public_git_history(
                &history.repo,
                &sink,
                &tips,
                &["feature", "main", "target"],
                ViewLimits::default(),
            )
            .expect("same union"),
        );
    }
    assert_eq!(mappings[0], mappings[1]);
    let out = tempfile::tempdir().expect("sink");
    let sink = GitRepository::init_bare(out.path()).expect("Git");
    for tips in [
        vec![],
        vec![HistoryTip {
            thread: "main",
            state: synthetic_initial_base().expect("seed").id(),
        }],
    ] {
        export_public_git_history(
            &history.repo,
            &sink,
            &tips,
            &["main"],
            ViewLimits::default(),
        )
        .expect_err("no fabricated empty Git root");
    }
    assert!(
        git(
            out.path(),
            &["cat-file", "--batch-all-objects", "--batch-check"]
        )
        .is_empty()
    );
}
