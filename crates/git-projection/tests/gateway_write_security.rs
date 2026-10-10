// SPDX-License-Identifier: Apache-2.0
//! Adversarial checks for the explicitly local Git-write core. No hosted authority is implied.

use crypto::Ed25519Signer;
use heddle_git_projection::{
    GitProjectionError,
    gateway_view::{HistoryTip, ViewLimits, export_public_git_history},
    gateway_write::{
        LocalPush, LocalWriter, PushUpdate, WriteLimits, accept_local_fixture_push,
        parse_receive_pack,
    },
};
use objects::{
    object::{
        Attribution, Blob, Principal, Redaction, StateId, StateVisibility, Tree, TreeEntry,
        VisibilityTier,
        thread_replication::{
            git_import_converter::{GitImportGraph, GitImportRawCommit},
            git_import_graph::{GitObjectFormat, GitObjectId},
        },
    },
    store::ObjectStore,
};
use repo::Repository;
use sley::{GitObjectType, ObjectId, Repository as GitRepository};
use std::{
    cell::Cell,
    io::Write,
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, Barrier},
};

struct Fixture {
    temp: tempfile::TempDir,
    native: Repository,
    quarantine: GitRepository,
    base: StateId,
    old: ObjectId,
    signer: Ed25519Signer,
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().expect("fixture");
    let native = Repository::init_default(temp.path().join("native")).expect("native repo");
    std::fs::write(native.root().join("base.txt"), b"public base\n").expect("base");
    let base = native
        .snapshot_with_attribution(
            Some("public base".into()),
            None,
            Attribution::human(Principal::new("Synthetic owner", "owner@example.invalid")),
        )
        .expect("base capture")
        .state_id;
    let quarantine =
        GitRepository::init_bare(temp.path().join("quarantine.git")).expect("quarantine");
    let mapping = export_public_git_history(
        &native,
        &quarantine,
        &[HistoryTip {
            thread: "main",
            state: base,
        }],
        &["main"],
        ViewLimits::default(),
    )
    .expect("public history");
    let old = mapping.get_git(&base).expect("old Git tip");
    let replica = native.native_thread("main").expect("main");
    let signer = native
        .native_thread_signer(&replica)
        .expect("existing synthetic fixture signer");
    Fixture {
        temp,
        native,
        quarantine,
        base,
        old,
        signer,
    }
}

fn git(path: &Path, args: &[&str], input: &[u8]) -> Vec<u8> {
    let mut child = Command::new("git")
        .env_clear()
        .envs([
            ("PATH", "/usr/bin:/bin"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_AUTHOR_NAME", "Untrusted Git author"),
            ("GIT_AUTHOR_EMAIL", "untrusted@example.invalid"),
            ("GIT_COMMITTER_NAME", "Untrusted Git author"),
            ("GIT_COMMITTER_EMAIL", "untrusted@example.invalid"),
            ("GIT_AUTHOR_DATE", "1700000000 +0000"),
            ("GIT_COMMITTER_DATE", "1700000000 +0000"),
        ])
        .arg("--git-dir")
        .arg(path)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Git fixture operation");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input)
        .expect("input");
    let output = child.wait_with_output().expect("Git output");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn commit(
    f: &Fixture,
    parent: Option<ObjectId>,
    name: &str,
    mode: &str,
    message: &str,
) -> ObjectId {
    let blob = f
        .quarantine
        .write_raw_object(GitObjectType::Blob, b"synthetic pushed bytes\n".to_vec())
        .expect("blob");
    let mut tree = format!("{mode} {name}\0").into_bytes();
    tree.extend_from_slice(blob.as_bytes());
    let tree = f
        .quarantine
        .write_raw_object(GitObjectType::Tree, tree)
        .expect("raw tree");
    let tree = tree.to_string();
    let mut args = vec!["commit-tree", &tree];
    let parent = parent.map(|id| id.to_string());
    if let Some(parent) = &parent {
        args.extend(["-p", parent]);
    }
    String::from_utf8(git(f.quarantine.git_dir(), &args, message.as_bytes()))
        .expect("OID")
        .trim()
        .parse()
        .expect("commit OID")
}

fn update(old: ObjectId, new: ObjectId) -> PushUpdate {
    PushUpdate {
        thread: "main".into(),
        old,
        new,
    }
}

fn heads(repo: &Repository) -> Vec<StateId> {
    repo.native_thread("main")
        .expect("main")
        .projection()
        .expect("projection")
        .source_heads
}

fn admit(
    f: &Fixture,
    update: &PushUpdate,
) -> heddle_git_projection::GitProjectionResult<
    heddle_git_projection::gateway_write::NativeAcceptance,
> {
    accept_local_fixture_push(
        &f.native,
        &f.quarantine,
        LocalPush {
            update,
            expected_native: f.base,
            policy_generation: "synthetic-policy-1",
        },
        LocalWriter {
            actor: "synthetic-writer",
            signer: &f.signer,
        },
        WriteLimits::default(),
        || Ok(()),
    )
}

fn receive_body(command: &str, caps: &str, count: u32) -> Vec<u8> {
    let command = format!("{command}\0{caps}");
    let mut body = format!("{:04x}{command}0000", command.len() + 4).into_bytes();
    body.extend_from_slice(b"PACK\0\0\0\x02");
    body.extend_from_slice(&count.to_be_bytes());
    body.extend_from_slice(&[0; 20]); // Decoder owns object/checksum validation, not framing parser.
    body
}

#[test]
fn receive_framing_rejects_forbidden_updates_and_capabilities() {
    let old = "a".repeat(40);
    let new = "b".repeat(40);
    let good = format!("{old} {new} refs/heads/main");
    assert!(
        parse_receive_pack(
            &receive_body(&good, "report-status ofs-delta object-format=sha1", 1),
            WriteLimits::default()
        )
        .is_ok()
    );
    for command in [
        format!("{} {new} refs/heads/main", "0".repeat(40)),
        format!("{old} {} refs/heads/main", "0".repeat(40)),
        format!("{old} {old} refs/heads/main"),
        format!("{old} {new} refs/tags/release"),
        format!("{old} {new} refs/notes/heddle"),
        format!("{old} {new} refs/replace/{old}"),
        format!("{old} {new} HEAD"),
        format!("{old} {new} refs/heads/../escape"),
        format!("{old} {new} refs/heads/main.lock"),
        format!("{old} {new} refs/heads/main extra"),
    ] {
        assert!(
            parse_receive_pack(
                &receive_body(&command, "report-status", 1),
                WriteLimits::default()
            )
            .is_err(),
            "{command}"
        );
    }
    for caps in [
        "",
        "ofs-delta",
        "report-status push-options",
        "report-status atomic",
        "report-status report-status",
        "report-status object-format=sha256",
        "report-status\0ofs-delta",
        "report-status agent=x\n",
    ] {
        assert!(
            parse_receive_pack(&receive_body(&good, caps, 1), WriteLimits::default()).is_err(),
            "{caps:?}"
        );
    }
    let mut multiple = receive_body(&good, "report-status", 1);
    let end = usize::from_str_radix(std::str::from_utf8(&multiple[..4]).expect("ASCII"), 16)
        .expect("length");
    multiple.splice(
        end..end + 4,
        format!("{:04x}{good}0000", good.len() + 4).bytes(),
    );
    assert!(parse_receive_pack(&multiple, WriteLimits::default()).is_err());
    assert!(
        parse_receive_pack(
            &receive_body(&good, "report-status", 20_001),
            WriteLimits::default()
        )
        .is_err()
    );
}

#[test]
fn successful_push_is_durable_exact_and_retry_is_actor_scoped() {
    let f = fixture();
    let new = commit(
        &f,
        Some(f.old),
        "pushed.txt",
        "100644",
        "ordinary Git work\n",
    );
    let update = update(f.old, new);
    let accepted = admit(&f, &update).expect("authorized local native acceptance");
    assert!(!accepted.replayed);
    assert_eq!(heads(&f.native), vec![accepted.receipt.native_state]);
    let replay = admit(&f, &update).expect("lost-ACK retry");
    assert!(replay.replayed);
    assert_eq!(replay.receipt, accepted.receipt);
    let reopened = Repository::open(f.native.root()).expect("reopen durable native state");
    assert_eq!(heads(&reopened), vec![accepted.receipt.native_state]);
    let out = tempfile::tempdir().expect("independent reconstructed Git");
    let sink = GitRepository::init_bare(out.path().join("fresh.git")).expect("fresh sink");
    let mapping = export_public_git_history(
        &reopened,
        &sink,
        &[HistoryTip {
            thread: "main",
            state: accepted.receipt.native_state,
        }],
        &["main"],
        ViewLimits::default(),
    )
    .expect("reconstruct native accepted history");
    assert_eq!(mapping.get_git(&accepted.receipt.native_state), Some(new));
    let other_actor = accept_local_fixture_push(
        &f.native,
        &f.quarantine,
        LocalPush {
            update: &update,
            expected_native: f.base,
            policy_generation: "synthetic-policy-1",
        },
        LocalWriter {
            actor: "other-writer",
            signer: &f.signer,
        },
        WriteLimits::default(),
        || Ok(()),
    );
    assert!(
        other_actor.is_err(),
        "another actor cannot borrow retry receipt"
    );
    let denied_retry = accept_local_fixture_push(
        &f.native,
        &f.quarantine,
        LocalPush {
            update: &update,
            expected_native: f.base,
            policy_generation: "synthetic-policy-1",
        },
        LocalWriter {
            actor: "synthetic-writer",
            signer: &f.signer,
        },
        WriteLimits::default(),
        || Err(GitProjectionError::Git("writer revoked".into())),
    );
    assert!(
        denied_retry.is_err(),
        "replay requires current authorization"
    );
}

#[test]
fn final_authorization_failure_rolls_back_native_heads_and_receipt() {
    let f = fixture();
    let new = commit(&f, Some(f.old), "new.txt", "100644", "normal change\n");
    let update = update(f.old, new);
    let calls = Cell::new(0);
    let result = accept_local_fixture_push(
        &f.native,
        &f.quarantine,
        LocalPush {
            update: &update,
            expected_native: f.base,
            policy_generation: "synthetic-policy-1",
        },
        LocalWriter {
            actor: "synthetic-writer",
            signer: &f.signer,
        },
        WriteLimits::default(),
        || {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                Ok(())
            } else {
                Err(GitProjectionError::Git(
                    "writer revoked before commit".into(),
                ))
            }
        },
    );
    assert!(result.is_err());
    assert_eq!(calls.get(), 2);
    assert_eq!(heads(&f.native), vec![f.base]);
    assert!(
        !admit(&f, &update)
            .expect("fresh retry after rollback")
            .replayed,
        "rejected transaction cannot leave success receipt"
    );
}

#[test]
fn wrong_signer_reserved_paths_native_trailers_and_new_roots_refuse() {
    let f = fixture();
    let wrong = Ed25519Signer::from_seed(&[7; 32]).expect("public synthetic test seed");
    let good = commit(&f, Some(f.old), "good.txt", "100644", "normal\n");
    let update = update(f.old, good);
    assert!(
        accept_local_fixture_push(
            &f.native,
            &f.quarantine,
            LocalPush {
                update: &update,
                expected_native: f.base,
                policy_generation: "synthetic-policy-1"
            },
            LocalWriter {
                actor: "synthetic-writer",
                signer: &wrong
            },
            WriteLimits::default(),
            || Ok(())
        )
        .is_err()
    );
    for name in [".git", ".GIT", "git~1", ".heddle", ".HEDDLE", "heddle~1"] {
        let new = commit(&f, Some(f.old), name, "100644", "normal\n");
        assert!(
            admit(&f, &self::update(f.old, new)).is_err(),
            "reserved name {name}"
        );
        assert_eq!(heads(&f.native), vec![f.base]);
    }
    let identity = commit(
        &f,
        Some(f.old),
        "good.txt",
        "100644",
        "forge native identity\n\nHeddle-Change: forged\n",
    );
    assert!(admit(&f, &self::update(f.old, identity)).is_err());
    let root = commit(&f, None, "good.txt", "100644", "unrelated root\n");
    assert!(admit(&f, &self::update(f.old, root)).is_err());
    assert_eq!(heads(&f.native), vec![f.base]);
}

#[test]
fn object_and_commit_budgets_fail_without_native_acceptance() {
    let f = fixture();
    let new = commit(&f, Some(f.old), "new.txt", "100644", "normal\n");
    let update = update(f.old, new);
    for limits in [
        WriteLimits {
            objects: 0,
            ..WriteLimits::default()
        },
        WriteLimits {
            commits: 0,
            ..WriteLimits::default()
        },
        WriteLimits {
            entries: 0,
            ..WriteLimits::default()
        },
        WriteLimits {
            decoded_bytes: 0,
            ..WriteLimits::default()
        },
        WriteLimits {
            object_bytes: 0,
            ..WriteLimits::default()
        },
    ] {
        assert!(
            accept_local_fixture_push(
                &f.native,
                &f.quarantine,
                LocalPush {
                    update: &update,
                    expected_native: f.base,
                    policy_generation: "synthetic-policy-1"
                },
                LocalWriter {
                    actor: "synthetic-writer",
                    signer: &f.signer
                },
                limits,
                || Ok(())
            )
            .is_err()
        );
        assert_eq!(heads(&f.native), vec![f.base]);
    }
}

#[test]
fn concurrent_same_head_writers_have_exactly_one_durable_winner() {
    let f = fixture();
    let left = commit(&f, Some(f.old), "left.txt", "100644", "left\n");
    let right = commit(&f, Some(f.old), "right.txt", "100644", "right\n");
    let barrier = Arc::new(Barrier::new(2));
    let mut workers = Vec::new();
    for (index, new) in [left, right].into_iter().enumerate() {
        let native = f.native.root().to_path_buf();
        let quarantine = f.quarantine.git_dir().to_path_buf();
        let barrier = barrier.clone();
        let old = f.old;
        let expected = f.base;
        workers.push(std::thread::spawn(move || {
            let native = Repository::open(native).expect("writer native handle");
            let quarantine = GitRepository::open(quarantine).expect("read-only quarantine handle");
            let signer = native
                .native_thread_signer(&native.native_thread("main").expect("main"))
                .expect("existing signer");
            let first = Cell::new(true);
            let actor = format!("writer-{index}");
            let update = self::update(old, new);
            accept_local_fixture_push(
                &native,
                &quarantine,
                LocalPush {
                    update: &update,
                    expected_native: expected,
                    policy_generation: "synthetic-policy-1",
                },
                LocalWriter {
                    actor: &actor,
                    signer: &signer,
                },
                WriteLimits::default(),
                || {
                    if first.replace(false) {
                        barrier.wait();
                    }
                    Ok(())
                },
            )
            .map(|accepted| accepted.receipt.native_state)
            .ok()
        }));
    }
    let winners: Vec<_> = workers
        .into_iter()
        .filter_map(|worker| worker.join().expect("writer thread"))
        .collect();
    assert_eq!(winners.len(), 1, "one expected-head CAS winner");
    assert_eq!(heads(&f.native), winners);
    assert!(f.temp.path().is_dir()); // Fixture outlives both threads and every native handle.
}

#[test]
fn previously_hidden_incoming_state_is_not_durably_admitted_as_public() {
    let f = fixture();
    let new = commit(
        &f,
        Some(f.old),
        "hidden.txt",
        "100644",
        "incoming hidden state\n",
    );
    let raw = f
        .quarantine
        .read_object(&new)
        .expect("raw Git commit")
        .body
        .clone();
    let blob = Blob::new(b"synthetic pushed bytes\n".to_vec());
    f.native
        .store()
        .put_blob(&blob)
        .expect("existing canonical blob");
    let tree = Tree::from_git_entries(vec![
        TreeEntry::file("hidden.txt", blob.hash(), false).expect("file"),
    ])
    .expect("native tree");
    f.native
        .store()
        .put_tree(&tree)
        .expect("existing canonical tree");
    let git_oid = GitObjectId::Sha1(new.as_bytes().try_into().expect("SHA-1"));
    let state = GitImportGraph::convert_raw_commit(
        GitImportRawCommit {
            oid: &git_oid,
            object_format: GitObjectFormat::Sha1,
            raw_commit: &raw,
            heddle_note: None,
        },
        tree.hash(),
        vec![f.base],
        false,
        |_| Ok(None),
    )
    .expect("canonical candidate");
    f.native
        .store()
        .put_state(&state)
        .expect("existing hidden state");
    f.native
        .put_state_visibility(StateVisibility {
            state: state.id(),
            tier: VisibilityTier::Private {
                scope_label: "security".into(),
            },
            embargo_until: None,
            declarer: state.attribution.principal.clone(),
            declared_at: chrono::Utc::now(),
            signature: None,
            supersedes: None,
        })
        .expect("current hidden policy");
    let error = admit(&f, &update(f.old, new))
        .err()
        .expect("hidden incoming state refused");
    assert!(error.to_string().contains("not public"), "{error}");
    assert_eq!(heads(&f.native), vec![f.base]);
    assert!(
        f.native
            .native_thread("main")
            .expect("main")
            .accepted_source_originals_for_revisions(&[state.id()])
            .expect("admitted originals")
            .is_empty()
    );
}

#[test]
fn redacted_blob_cannot_be_reintroduced_under_another_path() {
    let f = fixture();
    let new = commit(
        &f,
        Some(f.old),
        "renamed.txt",
        "100644",
        "same bytes new path\n",
    );
    let blob = Blob::new(b"synthetic pushed bytes\n".to_vec());
    f.native
        .put_redaction(Redaction {
            redacted_blob: blob.hash(),
            state: f.base,
            path: "prior-name.txt".into(),
            reason: "synthetic no-republish test".into(),
            redactor: Principal::new("Synthetic owner", "owner@example.invalid"),
            redacted_at: chrono::Utc::now(),
            signature: None,
            purge: None,
            supersedes: None,
        })
        .expect("blob-scoped redaction");
    let error = admit(&f, &update(f.old, new))
        .err()
        .expect("redacted bytes refused");
    assert!(error.to_string().contains("redacted blob"), "{error}");
    assert_eq!(heads(&f.native), vec![f.base]);
}

#[test]
fn complete_history_byte_budget_is_checked_before_native_acceptance() {
    let f = fixture();
    // Incoming decoded bytes count this shared object once; the public history
    // view counts its occurrence in every historical tree. Eight 9-MiB versions
    // must not fit through the 64-MiB publication budget by object deduplication.
    let blob = f
        .quarantine
        .write_raw_object(GitObjectType::Blob, vec![b'x'; 9 * 1024 * 1024])
        .expect("bounded shared blob");
    let mut tree = b"100644 repeated.bin\0".to_vec();
    tree.extend_from_slice(blob.as_bytes());
    let tree = f
        .quarantine
        .write_raw_object(GitObjectType::Tree, tree)
        .expect("shared tree")
        .to_string();
    let mut tip = f.old;
    for index in 0..8 {
        tip = String::from_utf8(git(
            f.quarantine.git_dir(),
            &["commit-tree", &tree, "-p", &tip.to_string()],
            format!("repeat shared tree {index}\n").as_bytes(),
        ))
        .expect("OID")
        .trim()
        .parse()
        .expect("commit");
    }
    let error = admit(&f, &update(f.old, tip))
        .err()
        .expect("combined history over budget must refuse before native commit");
    assert!(error.to_string().contains("byte limit"), "{error}");
    assert_eq!(heads(&f.native), vec![f.base]);
}

#[test]
fn complete_history_state_budget_includes_existing_ancestry() {
    let f = fixture();
    let mut tip = f.old;
    // Existing base + synthetic initial state + 127 incoming commits = 129.
    // Incoming commits alone are inside WriteLimits::default().commits (128).
    for index in 0..127 {
        tip = commit(
            &f,
            Some(tip),
            "public.txt",
            "100644",
            &format!("bounded new commit {index}\n"),
        );
    }
    let error = admit(&f, &update(f.old, tip))
        .err()
        .expect("combined history over budget must refuse before native commit");
    assert!(error.to_string().contains("state limit"), "{error}");
    assert_eq!(heads(&f.native), vec![f.base]);
}

#[test]
fn complete_history_entry_budget_includes_existing_trees() {
    let f = fixture();
    let blob = f
        .quarantine
        .write_raw_object(GitObjectType::Blob, b"shared\n".to_vec())
        .expect("shared blob");
    let mut tree = Vec::new();
    for index in 0..10_000 {
        tree.extend_from_slice(format!("100644 f{index:05}\0").as_bytes());
        tree.extend_from_slice(blob.as_bytes());
    }
    let tree = f
        .quarantine
        .write_raw_object(GitObjectType::Tree, tree)
        .expect("bounded incoming tree")
        .to_string();
    let tip = String::from_utf8(git(
        f.quarantine.git_dir(),
        &["commit-tree", &tree, "-p", &f.old.to_string()],
        b"incoming tree at its own entry limit\n",
    ))
    .expect("OID")
    .trim()
    .parse()
    .expect("commit");
    let error = admit(&f, &update(f.old, tip))
        .err()
        .expect("old plus new tree entries must fit publication budget before commit");
    assert!(error.to_string().contains("entry limit"), "{error}");
    assert_eq!(heads(&f.native), vec![f.base]);
}
