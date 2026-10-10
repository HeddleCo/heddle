// SPDX-License-Identifier: Apache-2.0
//! Public gateway disclosure checks against genuine local signed native captures.
//! These are local projection tests, not evidence of live hosted policy freshness.

use heddle_git_projection::gateway_view::{ViewLimits, export_public_native_view};
use objects::{
    object::{
        Attribution, Principal, Redaction, State, StateId, StateVisibility, TreeEntryTarget,
        VisibilityTier,
    },
    store::ObjectStore,
};
use repo::Repository;
use std::{io::Write, path::Path, process::Command};

fn actor() -> Attribution {
    Attribution::human(Principal::new(
        "Synthetic visibility test",
        "visibility@example.invalid",
    ))
}

fn fixture() -> (tempfile::TempDir, Repository, StateId) {
    let temp = tempfile::tempdir().expect("fixture");
    let repo = Repository::init_default(temp.path()).expect("native repo");
    std::fs::write(
        temp.path().join("source.txt"),
        b"Synthetic protected source\n",
    )
    .expect("fixture source");
    let state = repo
        .snapshot_with_attribution(Some("public base".into()), None, actor())
        .expect("signed capture");
    (temp, repo, state.state_id)
}

fn nonpublic_tiers() -> [VisibilityTier; 4] {
    [
        VisibilityTier::Internal,
        VisibilityTier::TeamScoped {
            team_id: "engineering".into(),
        },
        VisibilityTier::Restricted {
            scope_label: "security".into(),
        },
        VisibilityTier::Private {
            scope_label: "security".into(),
        },
    ]
}

fn narrow(repo: &Repository, state: StateId, tier: VisibilityTier, expired: bool) {
    repo.put_state_visibility(StateVisibility {
        state,
        tier,
        embargo_until: expired
            .then_some(chrono::DateTime::from_timestamp(1, 0).expect("past embargo time")),
        declarer: actor().principal,
        declared_at: chrono::Utc::now(),
        signature: None,
        supersedes: None,
    })
    .expect("signed narrowing");
}

fn git(path: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .arg("--git-dir")
        .arg(path)
        .args(args)
        .output()
        .expect("Git inspection")
}

fn refused_without_objects(repo: &Repository, state: StateId, snapshot: bool) -> String {
    let out = tempfile::tempdir().expect("sink fixture");
    let path = out.path().join("view.git");
    let sink = sley::Repository::init_bare(&path).expect("empty sink");
    let error =
        export_public_native_view(repo, &sink, state, "main", snapshot, ViewLimits::default())
            .expect_err("non-public closure refused");
    let objects = git(&path, &["cat-file", "--batch-all-objects", "--batch-check"]);
    assert!(objects.status.success(), "object inventory failed");
    assert!(
        objects.stdout.is_empty(),
        "refusal must precede writing any object"
    );
    error.to_string()
}

#[test]
fn current_state_narrowing_denies_every_nonpublic_tier_in_both_modes() {
    for tier in nonpublic_tiers() {
        let (_temp, repo, state) = fixture();
        narrow(&repo, state, tier, false);
        for snapshot in [true, false] {
            let error = refused_without_objects(&repo, state, snapshot);
            assert!(error.contains("state not publicly visible"), "{error}");
        }
    }
}

#[test]
fn public_child_cannot_launder_hidden_ancestor_through_snapshot_or_history() {
    for tier in nonpublic_tiers() {
        let (temp, repo, base) = fixture();
        std::fs::write(temp.path().join("child.txt"), b"public child\n").expect("child");
        let child = repo
            .snapshot_with_attribution(Some("public child".into()), None, actor())
            .expect("child capture");
        narrow(&repo, base, tier, false);
        assert_eq!(
            repo.effective_visibility_tier(&child.state_id)
                .expect("child tier"),
            VisibilityTier::Public
        );
        for snapshot in [true, false] {
            refused_without_objects(&repo, child.state_id, snapshot);
        }
    }
}

#[test]
fn original_private_capture_cannot_be_opened_by_removing_mutable_sidecar() {
    let (_temp, repo, base) = fixture();
    let tree = repo
        .store()
        .get_state(&base)
        .expect("state")
        .expect("base")
        .tree;
    let private = State::new_snapshot(tree, vec![base], actor()).with_intent("private capture");
    repo.put_authored_state(&private).expect("authored state");
    narrow(
        &repo,
        private.id(),
        VisibilityTier::Private {
            scope_label: "security".into(),
        },
        false,
    );
    repo.record_native_capture("main", private.id())
        .expect("signed private capture");
    repo.restore_state_visibility_sidecar(&private.id(), None)
        .expect("remove mutable sidecar");
    assert_eq!(
        repo.effective_visibility_tier(&private.id())
            .expect("local tier"),
        VisibilityTier::Public
    );
    for snapshot in [true, false] {
        let error = refused_without_objects(&repo, private.id(), snapshot);
        assert!(error.contains("original captured visibility"), "{error}");
    }
}

#[test]
fn elapsed_embargo_is_not_a_persisted_promotion() {
    let (_temp, repo, state) = fixture();
    narrow(
        &repo,
        state,
        VisibilityTier::Private {
            scope_label: "security".into(),
        },
        true,
    );
    for snapshot in [true, false] {
        refused_without_objects(&repo, state, snapshot);
    }
}

#[test]
fn signed_subtree_privacy_survives_mutable_sidecar_removal() {
    let (temp, repo, _base) = fixture();
    std::fs::create_dir(temp.path().join("restricted")).expect("directory");
    std::fs::write(
        temp.path().join("restricted/name.txt"),
        b"withheld name and bytes\n",
    )
    .expect("entry");
    repo.mark_subtree_visibility("restricted", VisibilityTier::Internal)
        .expect("subtree mark");
    let state = repo
        .snapshot_with_attribution(Some("private subtree".into()), None, actor())
        .expect("capture");
    repo.restore_entry_visibility_sidecar(&state.change_id, None)
        .expect("remove mutable sidecar");
    for snapshot in [true, false] {
        let error = refused_without_objects(&repo, state.state_id, snapshot);
        assert!(
            error.contains("original captured entry visibility"),
            "{error}"
        );
    }
}

#[test]
fn redaction_exports_stub_and_never_installs_original_blob_in_git_sink() {
    let (_temp, repo, state) = fixture();
    let native = repo
        .store()
        .get_state(&state)
        .expect("state lookup")
        .expect("state");
    let tree = repo
        .store()
        .get_tree(&native.tree)
        .expect("tree lookup")
        .expect("tree");
    let TreeEntryTarget::Blob { hash, .. } = tree.get("source.txt").expect("entry").target() else {
        panic!("source must be a blob");
    };
    let original = repo
        .store()
        .get_blob(hash)
        .expect("blob lookup")
        .expect("blob");
    repo.put_redaction(Redaction {
        redacted_blob: *hash,
        state,
        path: "source.txt".into(),
        reason: "synthetic visibility regression".into(),
        redactor: actor().principal,
        redacted_at: chrono::Utc::now(),
        signature: None,
        purge: None,
        supersedes: None,
    })
    .expect("signed redaction");
    for snapshot in [true, false] {
        let out = tempfile::tempdir().expect("sink fixture");
        let path = out.path().join("view.git");
        let sink = sley::Repository::init_bare(&path).expect("empty sink");
        let oid =
            export_public_native_view(&repo, &sink, state, "main", snapshot, ViewLimits::default())
                .expect("redacted public export");
        let shown = git(&path, &["show", &format!("{oid}:source.txt")]);
        assert!(shown.status.success());
        assert!(
            shown
                .stdout
                .starts_with(b"# This file was redacted by Heddle.")
        );
        assert!(
            !shown
                .stdout
                .windows(original.content().len())
                .any(|bytes| bytes == original.content())
        );
        let mut child = Command::new("git")
            .args(["hash-object", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("hash original bytes");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(original.content())
            .expect("write hash input");
        let hashed = child.wait_with_output().expect("hash result");
        assert!(hashed.status.success());
        let hidden_oid = String::from_utf8(hashed.stdout).expect("ASCII Git OID");
        assert!(
            !git(&path, &["cat-file", "-e", hidden_oid.trim()])
                .status
                .success()
        );
    }
    assert!(
        repo.store()
            .get_blob(hash)
            .expect("source retained")
            .is_some()
    );
}
